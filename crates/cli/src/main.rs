// SPDX-License-Identifier: Apache-2.0

//! bookrack command-line entry point.
//!
//! A thin shell over the library pipeline: parse arguments, resolve
//! [`Config`], install the tracing subscriber, open the stores, and call
//! the graduated `ingest_book` / `search` entry points. All orchestration
//! lives in those library functions; this binary only wires inputs to them
//! and renders their reports. Operational tuning comes entirely from the
//! environment via `Config` and the `*Config::from_env` resolvers — the
//! command surface carries no tuning flags, so there is a single source of
//! truth for every default.

mod dryrun;
mod remove;
mod render;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bookrack_catalog::{Catalog, IntakeFilter, STATUS_ACKNOWLEDGED, STATUS_PENDING};
use bookrack_config::{Config, EmbedConfig, LibrarySelection, LogConfig, SearchConfig};
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_extract::{Biblio, Provenance, TextLayerQuality};
use bookrack_ingest::{IngestParams, ingest_book, resume_from_chunk};
use bookrack_metadata::{AuditInput, AuditRules, TocStats};
use bookrack_ops::dto::BookFilter;
use bookrack_ops::{Caller, Ops, reads};
use bookrack_vectors::ChunkStore;

/// Trailing block shown by `bookrack --help`. Names the environment
/// variables that select the library and the embed backend, and the
/// runtime prerequisite a fresh install most often trips over: Ollama
/// must be reachable for any command that embeds text.
const TOP_AFTER_HELP: &str = "\
Environment:
  BOOKRACK_DATA_DIR     library data root (overridden by --data-dir)
  BOOKRACK_REGISTRY     TOML file mapping --library names to roots
  BOOKRACK_OLLAMA_URL   Ollama endpoint (default http://localhost:11434)
  BOOKRACK_EMBED_MODEL  embedding model tag (default qwen3-embedding:0.6b)
  BOOKRACK_LOG          tracing filter directive (default info; debug for verbose)

Prerequisites:
  ingest and query both call Ollama for embeddings. Start Ollama and pull
  the embed model before either command runs, e.g.:
      ollama pull qwen3-embedding:0.6b";

/// Trailing block shown by `bookrack ingest --help`.
const INGEST_AFTER_HELP: &str = "\
Examples:
  bookrack ingest path/to/book.epub
  bookrack ingest path/to/books-dir --recursive
  bookrack ingest path/to/book.epub --force";

/// Trailing block shown by `bookrack query --help`.
const QUERY_AFTER_HELP: &str = "\
Examples:
  bookrack query \"the history of madness\"
  bookrack query \"recurring motifs\" --in-book 1";

/// Trailing block shown by `bookrack remove --help`.
const REMOVE_AFTER_HELP: &str = "\
Examples:
  bookrack remove 42
  bookrack remove --sha 9f1c... --dry-run
  bookrack remove 42 --yes

Notes:
  metadata_audit and book_pipeline_audit rows are preserved by design
  so the pipeline history of a removed book remains queryable. Vector
  rows are tombstoned in LanceDB; their space is reclaimed by the
  optimize pass the next ingest runs, not by remove itself.";

#[derive(clap::Parser)]
#[command(
    name = "bookrack",
    version,
    about = "Search a local library of books.",
    after_help = TOP_AFTER_HELP,
)]
struct Cli {
    /// Operate on the library at this data root, overriding the
    /// environment. Mutually exclusive with `--library`.
    #[arg(long, global = true, conflicts_with = "library")]
    data_dir: Option<PathBuf>,
    /// Operate on the named library from the registry (see
    /// BOOKRACK_REGISTRY). Mutually exclusive with `--data-dir`.
    #[arg(long, global = true)]
    library: Option<String>,
    /// Select an audit profile by name. Built-in names are
    /// `default`, `trust-source`, and `strict`. Without this flag the
    /// `<data_root>/audit-rules/audit_profile.local.toml` overlay is
    /// merged onto the shipped default; with it the overlay is
    /// bypassed and the named preset wins.
    #[arg(long, global = true, value_name = "NAME")]
    audit_profile: Option<String>,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    /// The library selection these top-level flags express.
    fn selection(&self) -> LibrarySelection {
        LibrarySelection {
            data_dir: self.data_dir.clone(),
            library: self.library.clone(),
        }
    }
}

#[derive(clap::Subcommand)]
enum Command {
    /// Ingest and embed a single file (or, with `--recursive`, every
    /// supported file under a directory) into the library.
    #[command(after_help = INGEST_AFTER_HELP)]
    Ingest {
        /// Path to the source file, or — with `--recursive` — the
        /// directory to walk.
        path: PathBuf,
        /// Walk `path` as a directory, ingesting every supported file
        /// found. Files whose `source_sha256` is already registered are
        /// skipped via the existing intake deduplication; a per-file
        /// failure is logged and the walk continues.
        #[arg(long)]
        recursive: bool,
        /// Stop in the metadata stage when the audit verdict is
        /// `needs_work` and wait for an operator. Off by default —
        /// EMBED runs straight through and the audit verdict is
        /// merely advisory. With the flag on, the held book resumes
        /// through `bookrack metadata advance <book>` once an
        /// operator has corrected the record.
        #[arg(long)]
        hold_for_metadata: bool,
        /// Re-extract, re-chunk, and re-embed even when the source's
        /// `source_sha256` is already on file and every stamp matches
        /// this binary. Without this flag, an up-to-date re-ingest is a
        /// no-op. Use it after editing the source file or to recover
        /// from a corrupted partition.
        #[arg(long)]
        force: bool,
    },
    /// Query the library and print cited passages.
    #[command(after_help = QUERY_AFTER_HELP)]
    Query {
        /// The natural-language query.
        text: String,
        /// Restrict the recall to one book's id partition. Without the
        /// flag, every book in the library is in scope.
        #[arg(long, value_name = "INTAKE_ID")]
        in_book: Option<i64>,
        /// Force a brute-force scan for this query, ignoring any ANN
        /// index. Useful for ground-truth checks.
        #[arg(long)]
        bypass_ann: bool,
        /// Override the IVF probe count for this query only.
        #[arg(long)]
        nprobes: Option<usize>,
        /// Override the IVF-PQ refinement multiplier for this query only.
        #[arg(long)]
        refine_factor: Option<u32>,
    },
    /// Inspect and edit a book's metadata.
    Metadata {
        #[command(subcommand)]
        action: MetadataAction,
    },
    /// Simulate an ingest up to (but not including) embedding, and write
    /// a JSON report of what the metadata audit would have produced. The
    /// real catalog, corpus, and vector store are not touched.
    Dryrun {
        /// Source file, or a directory the dryrun walks recursively.
        path: PathBuf,
        /// Write the per-book report to this path instead of the default
        /// `<data_root>/dryruns/...` location. Implies the summary is
        /// written alongside with a `.summary.json` suffix.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write JSONL to stdout instead of to a file. The summary still
        /// lands on stderr at the end of the run.
        #[arg(long)]
        stdout: bool,
        /// Skip the CHUNK step. Saves seconds per large book when only
        /// the audit verdict is wanted.
        #[arg(long)]
        no_chunk: bool,
    },
    /// Manage the vector store's ANN index — inspect, rebuild, drop.
    Vectors {
        #[command(subcommand)]
        action: VectorsAction,
    },
    /// Manage the corpus database — rebuild it from the opaque store.
    Corpus {
        #[command(subcommand)]
        action: CorpusAction,
    },
    /// Render `book_pipeline_audit` rows for a book, oldest first.
    PipelineTrail {
        /// The intake id of the book.
        book: i64,
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Browse the library — list, find, show, table-of-contents, stats.
    Books {
        #[command(subcommand)]
        action: BooksAction,
    },
    /// Print a one-screen status card: resolution, embedder, schema
    /// versions, index stamps, and the on-disk size of each store.
    Info,
    /// Verify the catalog and corpus schemas against the binary's
    /// TableSpecs and tally the cross-store counts (catalog intakes,
    /// vectors-meta chunk count, intake-file existence on disk).
    Verify,
    /// Operate on the `corpus.db` `index_meta` stamps directly. The
    /// rebuild and reembed paths reconcile stamps as a side effect; this
    /// surface gives an operator a no-rebuild way to confirm — or fix —
    /// stamp drift.
    Stamps {
        #[command(subcommand)]
        action: StampsAction,
    },
    /// Inspect the library registry — the file named by
    /// `BOOKRACK_REGISTRY` that maps short names to data roots.
    Libraries {
        #[command(subcommand)]
        action: LibrariesAction,
    },
    /// Drop a book from every store — intake row, opaque envelope,
    /// corpus partition, vectors partition, and the cascaded catalog
    /// tables. Preserves `metadata_audit` and `book_pipeline_audit` as a
    /// forensic record. Vector rows are tombstoned; their space is
    /// reclaimed by the next ingest's optimize pass.
    #[command(after_help = REMOVE_AFTER_HELP)]
    Remove {
        /// Intake id of the book to remove. Mutually exclusive with
        /// `--sha`; exactly one of the two must be supplied.
        intake_id: Option<i64>,
        /// Whole-file SHA-256 of the source file, looked up in
        /// `catalog.intake.source_sha256`. Mutually exclusive with the
        /// positional intake id.
        #[arg(long, conflicts_with = "intake_id", value_name = "HEX")]
        sha: Option<String>,
        /// Print the per-store plan and exit without writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Subcommand)]
enum LibrariesAction {
    /// List every entry in the registry, marking the `default = "..."`
    /// fallback when one is set.
    List {
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
enum StampsAction {
    /// Probe the embedder for its vector dimension and write the
    /// resulting stamps into `corpus.db`'s `index_meta` when the table
    /// is unstamped. Fails on a stamp mismatch — the operator can then
    /// decide whether to rebuild.
    Reconcile,
}

#[derive(clap::Subcommand)]
enum BooksAction {
    /// List books in catalog order, paginated.
    List {
        /// Maximum books to print.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Skip this many books before printing.
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Filter books by title substring, contributor, or format.
    Find {
        /// Case-sensitive substring match against the book title.
        #[arg(long)]
        title: Option<String>,
        /// Exact-equality match against a contributor name.
        #[arg(long)]
        contributor: Option<String>,
        /// Restrict the contributor JOIN to one role (`author`,
        /// `translator`, ...). Only takes effect with `--contributor`.
        #[arg(long)]
        role: Option<String>,
        /// Exact-equality match against the file format.
        #[arg(long)]
        format: Option<String>,
        /// Maximum books to print.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Skip this many books before printing.
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Print the full bibliographic record for one book.
    Show {
        /// The intake id of the book.
        book: i64,
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Print one book's table of contents, depth-first.
    Toc {
        /// The intake id of the book.
        book: i64,
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Aggregate counts across the library.
    Stats {
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
enum VectorsAction {
    /// Print table size, ANN index state, and the persisted ANN config.
    Status,
    /// Build or rebuild the ANN index from explicit parameters. Without
    /// any flag, reads the persisted config from `vectors_meta.json` and
    /// rebuilds from that — useful after corpus growth has exceeded the
    /// L2 churn threshold.
    Rebuild {
        /// IVF family — `ivf-flat`, `ivf-sq`, `ivf-pq`, `ivf-hnsw-flat`,
        /// `ivf-hnsw-sq`, `ivf-hnsw-pq`. Defaults to whatever the meta
        /// holds, or `ivf-flat` for a fresh library.
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        num_partitions: Option<u32>,
        #[arg(long)]
        num_sub_vectors: Option<u32>,
        #[arg(long)]
        num_bits: Option<u32>,
        #[arg(long)]
        nprobes: Option<u32>,
        #[arg(long)]
        refine_factor: Option<u32>,
    },
    /// Drop the ANN index and mark the meta as brute-force. Search
    /// falls back to a full scan on the next query.
    Drop,
    /// Re-embed every (or a single) book's chunks in place: read the
    /// existing chunk rows back from LanceDB, drop their vectors, run
    /// them back through the active embedder, and write them as the
    /// new vectors. Use after switching `embed_model` or `embed_dim`.
    Reembed {
        /// Restrict the reembed to one intake id. Without this flag,
        /// every intake currently in the `Embedded` state is reembedded.
        #[arg(long, value_name = "INTAKE_ID")]
        book: Option<i64>,
        /// Restrict the reembed to intakes whose stored
        /// `extractor_version` does not equal this binary's
        /// `bookrack_extract::EXTRACTOR_VERSION`. Combines with `--book`
        /// by intersection.
        #[arg(long)]
        stale_only: bool,
        /// Print the plan (per-book chunk counts) and exit without
        /// writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Subcommand)]
enum CorpusAction {
    /// Rebuild `corpus.db` from the v1 extraction envelopes recorded in
    /// the opaque store. Intakes whose envelope is missing, mismatched,
    /// or corrupt are reported but skipped.
    Rebuild {
        /// After the corpus tree is rebuilt, also re-embed every
        /// reembedded book's chunks. Without this flag the LanceDB
        /// chunks table is left as-is — search still works because
        /// node ids are deterministic, but the vectors are unchanged.
        #[arg(long)]
        include_vectors: bool,
        /// Restrict the rebuild to one intake id. Without this flag,
        /// every intake whose lifecycle is past `Extracted` is rebuilt.
        #[arg(long, value_name = "INTAKE_ID")]
        book: Option<i64>,
        /// Restrict the rebuild to intakes whose stored
        /// `extractor_version` does not equal this binary's
        /// `bookrack_extract::EXTRACTOR_VERSION`. Combines with `--book`
        /// by intersection.
        #[arg(long)]
        stale_only: bool,
        /// Print the per-intake classification and exit without writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Subcommand)]
enum MetadataAction {
    /// Show the metadata audit report for a book.
    Show {
        /// The intake id of the book.
        book: i64,
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Set (or change) one metadata field's value.
    Set {
        /// The intake id of the book.
        book: i64,
        /// The field column on `node_publication_attrs` to write
        /// (e.g. `title`, `publisher`, `year`, `language`).
        field: String,
        /// The new value.
        value: String,
    },
    /// Clear an override, falling back to the extracted base value.
    Clear {
        /// The intake id of the book.
        book: i64,
        /// The field whose override is removed.
        field: String,
    },
    /// Acknowledge a metadata gap and let the book through, signing
    /// the override with a reason for the audit trail.
    Ack {
        /// The intake id of the book.
        book: i64,
        /// Why the gap was accepted.
        #[arg(long)]
        reason: String,
    },
    /// Mark the record reviewed and correct. A human or LLM uses this
    /// after confirming the metadata; the pipeline never writes this
    /// status itself.
    Approve {
        /// The intake id of the book.
        book: i64,
        /// Optional note for the audit trail.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Reject the book outright (e.g. wrong source file, irrecoverable
    /// metadata). The book stays ingested but downstream consumers can
    /// filter on the rejected status.
    Reject {
        /// The intake id of the book.
        book: i64,
        /// Why the book was rejected.
        #[arg(long)]
        reason: String,
    },
    /// Resume CHUNK→EMBED for a book held at the metadata gate.
    Advance {
        /// The intake id of the book.
        book: i64,
    },
    /// Inspect and compare audit profiles.
    AuditProfile {
        #[command(subcommand)]
        action: AuditProfileAction,
    },
    /// List books, optionally narrowed to those that still need review.
    List {
        /// Restrict the listing to books whose root audit confidence is
        /// `low` or `medium` *and* whose review status is `pending` or
        /// `acknowledged`. Missing review rows count as `pending`.
        #[arg(long)]
        needs_review: bool,
        /// Maximum rows to print.
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Skip this many rows before printing.
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Render the `metadata_audit` history for a book, oldest first.
    AuditTrail {
        /// The intake id of the book.
        book: i64,
        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
enum AuditProfileAction {
    /// Print every built-in profile name, one per line.
    List {
        /// Emit machine-readable JSON instead of the plain listing.
        #[arg(long)]
        json: bool,
    },
    /// Pretty-print the effective toggle settings for a named profile.
    Show {
        /// Built-in profile name (`default`, `trust-source`, `strict`).
        name: String,
    },
    /// List the sub-section names that differ between two named profiles
    /// and pretty-print each side's settings for those sections.
    Diff {
        /// First profile name.
        a: String,
        /// Second profile name.
        b: String,
    },
}

/// clap's default "did you mean" tip only sees top-level subcommand
/// names, so a user typing `bookrack list` lands on a suggestion of
/// `bookrack ingest`. This wrapper parses normally, then on a
/// `InvalidSubcommand` error checks the offending token against a
/// hand-maintained map of natural-name aliases and prints a friendlier
/// tip before exiting through clap's own renderer.
fn parse_cli_with_natural_name_hints() -> Cli {
    match <Cli as clap::Parser>::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if err.kind() == clap::error::ErrorKind::InvalidSubcommand
                && let Some(typed) = invalid_subcommand_token(&err)
                && let Some(hint) = natural_name_hint(&typed)
            {
                eprintln!("tip: did you mean {hint}?");
            }
            err.exit();
        }
    }
}

/// Pull the offending token out of clap's `InvalidSubcommand` error
/// context, or `None` if the context shape is unexpected.
fn invalid_subcommand_token(err: &clap::Error) -> Option<String> {
    err.context().find_map(|(kind, value)| {
        if matches!(kind, clap::error::ContextKind::InvalidSubcommand)
            && let clap::error::ContextValue::String(s) = value
        {
            Some(s.clone())
        } else {
            None
        }
    })
}

/// Map a natural-language guess at a command name to the real
/// invocation. Returns the hint string already shaped for the user
/// (multiple options joined with ` or `), or `None` for tokens not in
/// the table — those fall through to clap's own similarity tip.
fn natural_name_hint(typed: &str) -> Option<String> {
    let suggestions: &[&str] = match typed {
        "list" | "ls" => &["`bookrack books list`"],
        "find" => &["`bookrack books find <text>`"],
        "show" => &["`bookrack books show <id>`"],
        "stats" => &["`bookrack books stats`"],
        "status" => &["`bookrack info`", "`bookrack books stats`"],
        "search" => &["`bookrack query <text>`"],
        _ => return None,
    };
    Some(suggestions.join(" or "))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_cli_with_natural_name_hints();
    let cfg = Config::resolve(&cli.selection()).context("resolve configuration")?;
    let _guard = bookrack_obs::init(&cfg, &LogConfig::from_env());

    let profile_name = cli.audit_profile.clone();
    match cli.command {
        Command::Ingest {
            path,
            recursive,
            hold_for_metadata,
            force,
        } => {
            run_ingest(
                &cfg,
                &path,
                recursive,
                hold_for_metadata,
                force,
                profile_name.as_deref(),
            )
            .await
        }
        Command::Query {
            text,
            in_book,
            bypass_ann,
            nprobes,
            refine_factor,
        } => run_query(&cfg, &text, in_book, bypass_ann, nprobes, refine_factor).await,
        Command::Metadata { action } => run_metadata(&cfg, action, profile_name.as_deref()).await,
        Command::Dryrun {
            path,
            out,
            stdout,
            no_chunk,
        } => dryrun::run(
            &cfg,
            &path,
            out.as_deref(),
            stdout,
            no_chunk,
            profile_name.as_deref(),
        ),
        Command::Vectors { action } => match action {
            VectorsAction::Status => run_vectors_status(&cfg).await,
            VectorsAction::Rebuild {
                kind,
                num_partitions,
                num_sub_vectors,
                num_bits,
                nprobes,
                refine_factor,
            } => {
                run_vectors_rebuild(
                    &cfg,
                    kind.as_deref(),
                    num_partitions,
                    num_sub_vectors,
                    num_bits,
                    nprobes,
                    refine_factor,
                )
                .await
            }
            VectorsAction::Drop => run_vectors_drop(&cfg).await,
            VectorsAction::Reembed {
                book,
                stale_only,
                dry_run,
                yes,
            } => {
                run_vectors_reembed(
                    &cfg,
                    book,
                    stale_only,
                    dry_run,
                    yes,
                    profile_name.as_deref(),
                )
                .await
            }
        },
        Command::Corpus { action } => match action {
            CorpusAction::Rebuild {
                include_vectors,
                book,
                stale_only,
                dry_run,
                yes,
            } => {
                run_corpus_rebuild(
                    &cfg,
                    include_vectors,
                    book,
                    stale_only,
                    dry_run,
                    yes,
                    profile_name.as_deref(),
                )
                .await
            }
        },
        Command::PipelineTrail { book, json } => run_pipeline_trail(&cfg, book, json),
        Command::Books { action } => run_books(&cfg, action),
        Command::Info => run_info(&cfg).await,
        Command::Verify => run_verify(&cfg),
        Command::Stamps { action } => match action {
            StampsAction::Reconcile => run_stamps_reconcile(&cfg).await,
        },
        Command::Libraries { action } => match action {
            LibrariesAction::List { json } => run_libraries_list(json),
        },
        Command::Remove {
            intake_id,
            sha,
            dry_run,
            yes,
        } => {
            remove::run(
                &cfg,
                remove::RemoveArgs {
                    intake_id,
                    sha,
                    dry_run,
                    yes,
                },
            )
            .await
        }
    }
}

fn run_libraries_list(json: bool) -> Result<()> {
    let entries = bookrack_config::list_libraries().context("list libraries")?;
    if json {
        render::libraries_list_json(entries.as_deref());
    } else {
        render::libraries_list(entries.as_deref());
    }
    Ok(())
}

async fn run_stamps_reconcile(cfg: &Config) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let embedder = embedder(cfg, &embed_cfg)?;
    // Probe the embedder once for its current vector dimension. The
    // probe is the only network call this command makes; the corpus
    // write happens locally.
    let probe = embedder
        .embed_batch(&["dimension probe".to_string()])
        .await
        .context("probe embedding dimension")?;
    let dim = probe
        .first()
        .map(Vec::len)
        .context("embedder returned no vector")?;
    let stamps = bookrack_ingest::current_index_stamps(&embed_cfg.model, dim as u32);
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    corpus
        .reconcile_index_stamps(&stamps)
        .context("reconcile index stamps")?;
    println!(
        "stamps reconciled: embed_model={} vector_dim={} chunk_version={} normalize_version={}",
        stamps.embed_model, stamps.vector_dim, stamps.chunk_version, stamps.normalize_version,
    );
    Ok(())
}

fn run_verify(cfg: &Config) -> Result<()> {
    let report = build_verify_report(cfg);
    render::verify(&report);
    if report.catalog_schema_error.is_some() || report.corpus_schema_error.is_some() {
        anyhow::bail!("one or more stores failed verification");
    }
    Ok(())
}

/// Collect verifiable findings for every store under `cfg`. A data
/// directory whose `catalog.db` does not yet exist is reported as
/// `not_initialised` and no stores are opened, so verify stays
/// side-effect-free on a freshly created directory.
fn build_verify_report(cfg: &Config) -> render::VerifyReport {
    let mut report = render::VerifyReport::default();

    if !cfg.catalog_db().exists() {
        report.not_initialised = true;
        return report;
    }

    // Schema verification happens inside the open paths; surface success
    // as a one-liner per database, and any failure as a multi-line block.
    match Catalog::open_read_only(&cfg.catalog_db()) {
        Ok(catalog) => {
            report.catalog_schema_ok = true;
            report.intake_count = catalog.count_intakes().ok();
            report.missing_intake_files = scan_intake_files(cfg, &catalog).ok();
        }
        Err(e) => {
            report.catalog_schema_error = Some(format!("{e:#}"));
        }
    }
    match Corpus::open(&cfg.corpus_db()) {
        Ok(_) => {
            report.corpus_schema_ok = true;
        }
        Err(e) => {
            report.corpus_schema_error = Some(format!("{e:#}"));
        }
    }
    let vectors_meta = bookrack_vectors::meta::load(&cfg.lancedb_dir())
        .ok()
        .flatten();
    if let Some(meta) = &vectors_meta {
        report.vectors_built_at_chunk_count = Some(meta.built_at_chunk_count);
        report.vectors_churn = Some(meta.churn_since_rebuild);
    }
    report
}

/// Walk every intake row, resolve its `stored_path` under `books/`, and
/// return the intake ids whose file is missing. `None` is returned only
/// when the catalog could not be enumerated.
fn scan_intake_files(cfg: &Config, catalog: &Catalog) -> Result<Vec<i64>> {
    let intakes = catalog
        .find_intakes(&IntakeFilter::default(), u32::MAX, 0)
        .context("enumerate intakes")?;
    let books_root = cfg.books_dir();
    let mut missing = Vec::new();
    for intake in intakes {
        let Some(stored) = intake.stored_path else {
            continue;
        };
        let resolved = books_root.join(&stored);
        if !resolved.exists() {
            missing.push(intake.intake_id);
        }
    }
    Ok(missing)
}

async fn run_info(cfg: &Config) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let corpus_stamps = read_corpus_stamps(cfg).unwrap_or_default();
    let vectors_meta = bookrack_vectors::meta::load(&cfg.lancedb_dir())
        .ok()
        .flatten();
    let current_chunks = read_current_chunk_count(cfg, &corpus_stamps).await;
    let intake_count = open_read_only_catalog(cfg)
        .and_then(|c| c.count_intakes().map_err(anyhow::Error::from))
        .ok();
    let ready_count = open_read_only_catalog(cfg)
        .and_then(|c| {
            c.count_book_states_by_stage("ready")
                .map_err(anyhow::Error::from)
        })
        .ok();
    let disk = disk_usage(cfg);
    let snapshot = render::InfoSnapshot {
        data_dir: cfg.data_dir().display().to_string(),
        library: cfg.library().map(str::to_string),
        source: resolution_source_label(cfg.source()),
        ollama_url: cfg.ollama_url().to_string(),
        embed_model_configured: embed_cfg.model.clone(),
        corpus_schema_version_expected: bookrack_corpus::SCHEMA_VERSION,
        catalog_schema_version_expected: bookrack_catalog::SCHEMA_VERSION,
        corpus_stamps,
        vectors_meta,
        current_chunks,
        intake_count,
        ready_book_count: ready_count,
        disk,
    };
    render::info(&snapshot);
    Ok(())
}

/// Best-effort live read of the vector store's row count. The corpus
/// stamp pins the dimension the store was built with; a library that
/// has never been ingested into has no stamp, no store, and no
/// answer — return `None` and let the renderer say so. Errors are
/// swallowed so `info` stays informational rather than failing on a
/// half-built library.
async fn read_current_chunk_count(
    cfg: &Config,
    corpus_stamps: &render::CorpusStamps,
) -> Option<usize> {
    let dim: usize = corpus_stamps.vector_dim.as_deref()?.parse().ok()?;
    let store = ChunkStore::open(&cfg.lancedb_dir(), dim).await.ok()?;
    store.count_rows().await.ok()
}

fn open_read_only_catalog(cfg: &Config) -> Result<Catalog> {
    Catalog::open_read_only(&cfg.catalog_db()).map_err(anyhow::Error::from)
}

fn read_corpus_stamps(cfg: &Config) -> Result<render::CorpusStamps> {
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    Ok(render::CorpusStamps {
        embed_model: corpus
            .meta_get(bookrack_corpus::EMBED_MODEL_KEY)
            .ok()
            .flatten(),
        vector_dim: corpus
            .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
            .ok()
            .flatten(),
        chunk_version: corpus
            .meta_get(bookrack_corpus::CHUNK_VERSION_KEY)
            .ok()
            .flatten(),
        normalize_version: corpus
            .meta_get(bookrack_corpus::NORMALIZE_VERSION_KEY)
            .ok()
            .flatten(),
        schema_version_on_disk: corpus.meta_get("schema_version").ok().flatten(),
    })
}

fn resolution_source_label(source: bookrack_config::ResolutionSource) -> &'static str {
    use bookrack_config::ResolutionSource::*;
    match source {
        DataDirFlag => "--data-dir flag",
        LibraryFlag => "--library flag",
        EnvVar => "BOOKRACK_DATA_DIR env",
        RegistryDefault => "registry default",
        Explicit => "explicit",
    }
}

fn disk_usage(cfg: &Config) -> render::DiskUsage {
    render::DiskUsage {
        catalog_db: file_size(&cfg.catalog_db()),
        corpus_db: file_size(&cfg.corpus_db()),
        lancedb_dir: dir_size(&cfg.lancedb_dir()),
    }
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn dir_size(path: &Path) -> Option<u64> {
    if !path.is_dir() {
        return None;
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    Some(total)
}

fn run_books(cfg: &Config, action: BooksAction) -> Result<()> {
    let ops = catalog_only_ops(cfg);
    match action {
        BooksAction::List {
            limit,
            offset,
            json,
        } => run_books_list(&ops, BookFilter::default(), limit, offset, json),
        BooksAction::Find {
            title,
            contributor,
            role,
            format,
            limit,
            offset,
            json,
        } => {
            let filter = BookFilter {
                title_substring: title,
                contributor_name: contributor,
                contributor_role: role,
                statuses: Vec::new(),
                format,
            };
            run_books_list(&ops, filter, limit, offset, json)
        }
        BooksAction::Show { book, json } => run_books_show(&ops, book, json),
        BooksAction::Toc { book, json } => run_books_toc(&ops, book, json),
        BooksAction::Stats { json } => run_books_stats(&ops, json),
    }
}

fn run_books_list(
    ops: &Ops<OllamaEmbedClient>,
    filter: BookFilter,
    limit: u32,
    offset: u32,
    json: bool,
) -> Result<()> {
    let result =
        reads::books::find_books(ops, filter, limit, offset).context("find books via ops")?;
    if json {
        render::books_list_json(&result);
    } else {
        render::books_list(&result);
    }
    Ok(())
}

fn run_books_show(ops: &Ops<OllamaEmbedClient>, book: i64, json: bool) -> Result<()> {
    let detail = match reads::books::show_book(ops, book) {
        Ok(d) => d,
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => return Err(anyhow::Error::from(e).context("show book via ops")),
    };
    if json {
        render::books_show_json(&detail);
    } else {
        render::books_show(&detail);
    }
    Ok(())
}

fn run_books_toc(ops: &Ops<OllamaEmbedClient>, book: i64, json: bool) -> Result<()> {
    let toc = match reads::books::show_toc(ops, book) {
        Ok(t) => t,
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => return Err(anyhow::Error::from(e).context("show toc via ops")),
    };
    if toc.nodes.is_empty() {
        if json {
            println!("null");
        } else {
            println!("Book {book}: no TOC nodes.");
        }
        return Ok(());
    }
    if json {
        render::books_toc_json(&toc);
    } else {
        render::books_toc(&toc);
    }
    Ok(())
}

fn run_books_stats(ops: &Ops<OllamaEmbedClient>, json: bool) -> Result<()> {
    let stats = reads::books::show_stats(ops).context("show stats via ops")?;
    if json {
        render::books_stats_json(&stats);
    } else {
        render::books_stats(&stats);
    }
    Ok(())
}

/// Build a catalog-only [`Ops`] for short-lived CLI invocations that do
/// not need vector search. Skips the Ollama dimension probe so the
/// process can serve a `books *` subcommand in milliseconds.
fn catalog_only_ops(cfg: &Config) -> Ops<OllamaEmbedClient> {
    Ops::catalog_only(
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        Caller::cli(),
    )
}

/// Render `bookrack vectors status` — a read-only summary of the
/// table, the LanceDB index it carries, and the persisted ANN config.
async fn run_vectors_status(cfg: &Config) -> Result<()> {
    // Read the vector dimension from corpus stamps. Absent stamps mean
    // the library has never been ingested into; the vector table will
    // not exist on disk either, so the output is the "empty" form.
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let dim = match corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
    {
        Some(s) => s.parse::<usize>().context("parse vector_dim stamp")?,
        None => {
            println!("table:           (empty — no chunks ingested yet)");
            println!("ann index:       (none)");
            println!("ann config:      (no meta)");
            println!("churn:           n/a");
            return Ok(());
        }
    };
    let lancedb_dir = cfg.lancedb_dir();
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open vector store")?;
    let row_count = store.count_rows().await.context("count rows")?;
    let indices = store.list_indices().await.context("list indices")?;
    let ann_cfg = store
        .current_ann_cfg(&lancedb_dir)
        .context("read ann config")?;
    let meta = bookrack_vectors::meta::load(&lancedb_dir).context("load vectors_meta")?;
    print_status(row_count, &indices, &store, &ann_cfg, &meta).await?;
    Ok(())
}

/// Write the status output to stdout. Split out so a future test can
/// drive the renderer with a fixed `StatusInputs` and assert against
/// the string — for now the command exercises it end-to-end.
async fn print_status(
    row_count: usize,
    indices: &[String],
    store: &ChunkStore,
    ann_cfg: &Option<bookrack_vectors::AnnConfig>,
    meta: &Option<bookrack_vectors::VectorsMeta>,
) -> Result<()> {
    println!("table:           {row_count} rows");
    // LanceDB has been observed to enumerate the same index name more
    // than once after repeated ingest / optimize passes. Print each
    // distinct name once, preserving the order they were reported in.
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<&str> = indices
        .iter()
        .filter(|n| seen.insert(n.as_str()))
        .map(String::as_str)
        .collect();
    if unique.is_empty() {
        println!("ann index:       (none — brute-force)");
    } else {
        for name in &unique {
            println!("ann index:       {name}");
            let stats = store
                .index_stats(name)
                .await
                .with_context(|| format!("index_stats({name})"))?;
            if let Some(s) = stats {
                println!("  type:          {:?}", s.index_type);
                println!("  num_indexed:   {}", s.num_indexed_rows);
                println!("  num_unindexed: {}", s.num_unindexed_rows);
                if let Some(ni) = s.num_indices {
                    println!("  num_indices:   {ni}");
                }
                if let Some(loss) = s.loss {
                    println!("  loss:          {loss}");
                } else {
                    println!("  loss:          n/a");
                }
            }
        }
    }
    match ann_cfg {
        None => println!("ann config:      (no meta)"),
        Some(c) => println!(
            "ann config:      {} / np={} / nprobes={} / refine={}",
            c.kind.as_str(),
            c.num_partitions,
            c.nprobes,
            c.refine_factor
                .map(|r| r.to_string())
                .unwrap_or_else(|| "n/a".to_string())
        ),
    }
    match meta {
        None => println!("churn:           n/a"),
        Some(m) => println!(
            "churn:           {} since last rebuild",
            m.churn_since_rebuild
        ),
    }
    // Meta drift: the meta claims an index name that LanceDB does not
    // actually carry. This is the visible after-effect of a failed
    // rebuild (meta written, but later state diverged) or of a manual
    // intervention on the lancedb directory. Suggest a rebuild — the
    // two sides reconcile from a fresh build.
    if let Some(m) = meta
        && m.kind != "brute-force"
        && !indices.contains(&m.lance_index_name)
    {
        println!(
            "meta drift:      expected index {:?}, found {:?}; \
             run bookrack vectors rebuild",
            m.lance_index_name, indices
        );
    }
    Ok(())
}

/// Render `bookrack vectors rebuild` — build or rebuild the ANN index
/// from CLI flags, falling back to the persisted meta or the C1
/// recommended default for any flag not supplied.
#[allow(clippy::too_many_arguments)]
async fn run_vectors_rebuild(
    cfg: &Config,
    kind_str: Option<&str>,
    num_partitions: Option<u32>,
    num_sub_vectors: Option<u32>,
    num_bits: Option<u32>,
    nprobes: Option<u32>,
    refine_factor: Option<u32>,
) -> Result<()> {
    let lancedb_dir = cfg.lancedb_dir();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or_else(|| {
            anyhow::anyhow!("library has no ingested chunks yet; ingest a book before rebuild")
        })?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open vector store")?;
    // Pick the baseline: explicit kind > existing meta > default IvfFlat.
    let mut base = if let Some(s) = kind_str {
        let kind: bookrack_vectors::AnnKind =
            s.parse().with_context(|| format!("parse --kind {s:?}"))?;
        bookrack_vectors::AnnConfig::default_for(kind)
    } else if let Some(c) = store
        .current_ann_cfg(&lancedb_dir)
        .context("read ann config")?
    {
        c
    } else {
        bookrack_vectors::AnnConfig::default_for(bookrack_vectors::AnnKind::IvfFlat)
    };
    if let Some(v) = num_partitions {
        base.num_partitions = v;
    }
    if let Some(v) = num_sub_vectors {
        base.num_sub_vectors = Some(v);
    }
    if let Some(v) = num_bits {
        base.num_bits = Some(v);
    }
    if let Some(v) = nprobes {
        base.nprobes = v;
    }
    if let Some(v) = refine_factor {
        base.refine_factor = Some(v);
    }
    store
        .build_ann_index(&base, &lancedb_dir, bookrack_ingest::now_rfc3339())
        .await
        .context("build ann index")?;
    println!(
        "rebuilt: kind={} np={}",
        base.kind.as_str(),
        base.num_partitions
    );
    Ok(())
}

/// Render `bookrack vectors reembed` — read each book's chunks back
/// from LanceDB, drop the vectors, and run the active embedder over
/// them. Use after switching `embed_model` / `embed_dim`.
async fn run_vectors_reembed(
    cfg: &Config,
    book: Option<i64>,
    stale_only: bool,
    dry_run: bool,
    yes: bool,
    profile_name: Option<&str>,
) -> Result<()> {
    let lancedb_dir = cfg.lancedb_dir();
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let plans = bookrack_ingest::reembed::plan_reembed(&catalog, &lancedb_dir, book, stale_only)
        .await
        .context("plan reembed")?;
    if plans.is_empty() {
        if stale_only {
            println!("no stale partitions; nothing to reembed");
        } else {
            println!("no embedded intakes carry chunks; nothing to reembed");
        }
        return Ok(());
    }
    let total_chunks: usize = plans.iter().map(|p| p.chunk_count).sum();
    let total_chars: usize = plans.iter().map(|p| p.total_chars).sum();
    println!("reembed plan ({} intakes):", plans.len());
    for plan in &plans {
        println!(
            "  intake {:>4}: {:>5} chunks, {:>9} chars",
            plan.intake_id, plan.chunk_count, plan.total_chars
        );
    }
    println!(
        "totals:        {:>5} chunks, {:>9} chars",
        total_chunks, total_chars
    );
    if dry_run {
        return Ok(());
    }
    let prompt = "About to delete-and-rewrite the chunk rows above.\n\
                  Existing vectors will be overwritten by fresh embeddings\n\
                  from the currently configured model. This is irreversible.\n\
                  Type 'yes' to continue: ";
    if !yes && !confirm(prompt)? {
        println!("aborted; no changes written");
        return Ok(());
    }

    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let _ = profile_name;
    let embed_cfg = EmbedConfig::from_env();
    let embedder_client = embedder(cfg, &embed_cfg)?;
    let report = bookrack_ingest::reembed::reembed_all(
        &catalog,
        &corpus,
        &lancedb_dir,
        &embed_cfg,
        &embedder_client,
        book,
        stale_only,
    )
    .await
    .context("reembed_all")?;
    let _ = &mut corpus;

    let total_written: usize = report
        .intakes
        .iter()
        .map(|o| o.embed_run.chunks_written)
        .sum();
    let total_batches: usize = report.intakes.iter().map(|o| o.embed_run.batches).sum();
    let total_shrinks: usize = report
        .intakes
        .iter()
        .map(|o| o.embed_run.shrink_events)
        .sum();
    println!(
        "reembedded: {} intakes / {} chunks / {} batches / {} shrinks",
        report.intakes.len(),
        total_written,
        total_batches,
        total_shrinks
    );
    if !report.skipped_empty.is_empty() {
        println!("skipped (no chunks): {:?}", report.skipped_empty);
    }
    Ok(())
}

/// Render `bookrack corpus rebuild` — regenerate `corpus.db` nodes
/// from the v1 extraction envelopes recorded in the opaque store.
async fn run_corpus_rebuild(
    cfg: &Config,
    include_vectors: bool,
    book: Option<i64>,
    stale_only: bool,
    dry_run: bool,
    yes: bool,
    profile_name: Option<&str>,
) -> Result<()> {
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;

    let plan_params = bookrack_ingest::rebuild::RebuildParams {
        only: book,
        stale_only,
        dry_run: true,
        ..Default::default()
    };
    let plan_report =
        bookrack_ingest::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &plan_params)
            .context("plan rebuild")?;
    println!(
        "rebuild plan: {} rebuildable, {} missing_envelope, {} mismatched, {} failed",
        plan_report.rebuilt.len(),
        plan_report.missing_envelope.len(),
        plan_report.mismatched.len(),
        plan_report.failed.len()
    );
    if !plan_report.missing_envelope.is_empty() {
        println!("  missing_envelope: {:?}", plan_report.missing_envelope);
    }
    if !plan_report.mismatched.is_empty() {
        println!("  mismatched:       {:?}", plan_report.mismatched);
    }
    if !plan_report.failed.is_empty() {
        for (id, err) in &plan_report.failed {
            println!("  failed:           intake {id}: {err}");
        }
    }
    if dry_run {
        return Ok(());
    }
    if plan_report.rebuilt.is_empty() {
        println!("no rebuildable intakes; aborting");
        return Ok(());
    }

    let prompt = if include_vectors {
        "About to overwrite corpus.db node rows for the intakes above,\n\
         then re-embed each book's chunks into LanceDB. This is\n\
         irreversible (the existing corpus tree is replaced).\n\
         Type 'yes' to continue: "
    } else {
        "About to overwrite corpus.db node rows for the intakes above.\n\
         LanceDB will retain its current chunks; the index_meta build\n\
         stamps are re-stamped from the existing chunks so search can\n\
         continue to run. Re-embed with `bookrack vectors reembed`\n\
         if you bumped the chunking or normalization algorithm.\n\
         This is irreversible (the existing corpus tree is replaced).\n\
         Type 'yes' to continue: "
    };
    if !yes && !confirm(prompt)? {
        println!("aborted; no changes written");
        return Ok(());
    }

    let run_params = bookrack_ingest::rebuild::RebuildParams {
        only: book,
        stale_only,
        dry_run: false,
        ..Default::default()
    };
    let report = bookrack_ingest::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &run_params)
        .context("rebuild")?;
    println!(
        "rebuilt: {} intakes ({} missing_envelope, {} mismatched, {} failed)",
        report.rebuilt.len(),
        report.missing_envelope.len(),
        report.mismatched.len(),
        report.failed.len()
    );

    // L0-only rebuilds end here with a fresh node tree but no
    // index_meta stamps; the next `query` would fail at the
    // serve-side gate. Re-stamp from the existing chunks before
    // returning so search keeps working. When `--include-vectors`
    // is set the subsequent reembed writes the same stamps, so this
    // path is skipped to avoid a redundant reconcile.
    if !include_vectors && !report.rebuilt.is_empty() {
        let lancedb_dir = cfg.lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let stamped = bookrack_ingest::rebuild::stamp_index_from_existing_chunks(
            &corpus,
            &lancedb_dir,
            &embed_cfg.model,
        )
        .await
        .context("stamp index_meta after rebuild")?;
        if !stamped {
            println!(
                "no chunks on disk; index_meta stamps were not written. \
                 Run `bookrack vectors reembed` after embedding to enable search."
            );
        }
    }

    if include_vectors && !report.rebuilt.is_empty() {
        let lancedb_dir = cfg.lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let embedder_client = embedder(cfg, &embed_cfg)?;
        let _ = profile_name;
        let reembed = bookrack_ingest::reembed::reembed_all(
            &catalog,
            &corpus,
            &lancedb_dir,
            &embed_cfg,
            &embedder_client,
            book,
            stale_only,
        )
        .await
        .context("reembed after rebuild")?;
        let total_written: usize = reembed
            .intakes
            .iter()
            .map(|o| o.embed_run.chunks_written)
            .sum();
        println!(
            "reembedded: {} intakes / {} chunks",
            reembed.intakes.len(),
            total_written
        );
    }
    Ok(())
}

/// Read a confirmation token from stdin: only the literal "yes"
/// (case-insensitive, trimmed) passes.
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{Write, stdin, stdout};
    print!("{prompt}");
    stdout().flush().context("flush stdout")?;
    let mut buf = String::new();
    stdin().read_line(&mut buf).context("read confirmation")?;
    Ok(buf.trim().eq_ignore_ascii_case("yes"))
}

/// Render `bookrack vectors drop` — drop any ANN index and stamp the
/// meta as `kind = brute-force`. Search falls back to a full scan.
async fn run_vectors_drop(cfg: &Config) -> Result<()> {
    let lancedb_dir = cfg.lancedb_dir();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or_else(|| anyhow::anyhow!("library has no ingested chunks yet; nothing to drop"))?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open vector store")?;
    store
        .drop_ann_index(&lancedb_dir, bookrack_ingest::now_rfc3339())
        .await
        .context("drop ann index")?;
    println!("dropped: kind=brute-force");
    Ok(())
}

/// Build the embedding client from the environment-resolved knobs.
fn embedder(cfg: &Config, embed_cfg: &EmbedConfig) -> Result<OllamaEmbedClient> {
    OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build embedding client")
}

/// Load the metadata audit's runtime rule set from
/// `cfg.audit_rules_dir()`. A missing directory or malformed file is
/// logged and yields an empty set; the audit then treats every value
/// as neutral.
pub(crate) fn load_audit_rules(cfg: &Config) -> AuditRules {
    match AuditRules::load_from(&cfg.audit_rules_dir()) {
        Ok(rules) => rules,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load audit rules; using empty set");
            AuditRules::empty()
        }
    }
}

/// Resolve the active audit profile.
///
/// When `name` is `Some`, the named built-in (`default` /
/// `trust-source` / `strict`) is returned; an unknown name falls
/// through to the overlay path. When `name` is `None`, the shipped
/// default is loaded and merged with any
/// `<data_root>/audit-rules/audit_profile.local.toml` overlay. A
/// malformed overlay is logged and the in-repo default is used as-is.
pub(crate) fn load_audit_profile(
    cfg: &Config,
    name: Option<&str>,
) -> bookrack_metadata::AuditProfile {
    if let Some(label) = name
        && let Some(named) = bookrack_metadata::AuditProfile::from_named(label)
    {
        return named;
    }
    match bookrack_metadata::AuditProfile::load_from(&cfg.audit_rules_dir()) {
        Ok(profile) => profile,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load audit profile overlay; using shipped default",
            );
            bookrack_metadata::AuditProfile::default_profile()
        }
    }
}

async fn run_ingest(
    cfg: &Config,
    path: &Path,
    recursive: bool,
    hold_for_metadata: bool,
    force: bool,
    profile_name: Option<&str>,
) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let embedder = embedder(cfg, &embed_cfg)?;
    let audit_rules = load_audit_rules(cfg);
    let audit_profile = load_audit_profile(cfg, profile_name);
    let params = IngestParams {
        embed: embed_cfg,
        hold_for_metadata,
        force,
        audit_rules,
        audit_profile,
        ..Default::default()
    };

    if !recursive {
        if path.is_dir() {
            anyhow::bail!(
                "{} is a directory; pass --recursive to walk it instead",
                path.display(),
            );
        }
        let report = ingest_book(
            path,
            &mut corpus,
            &mut catalog,
            &cfg.lancedb_dir(),
            &cfg.books_dir(),
            &embedder,
            &params,
        )
        .await
        .context("ingest book")?;
        render::ingest(&report);
        return Ok(());
    }

    if !path.is_dir() {
        anyhow::bail!(
            "--recursive requires a directory; {} is not one",
            path.display()
        );
    }
    let files = collect_supported_files(path)?;
    if files.is_empty() {
        println!("No supported files under {}.", path.display());
        return Ok(());
    }
    println!(
        "Walking {} ({} supported file{}):",
        path.display(),
        files.len(),
        if files.len() == 1 { "" } else { "s" },
    );
    let mut newly_ingested = 0usize;
    let mut refreshed = 0usize;
    let mut skipped_noop = 0usize;
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    for file in &files {
        match ingest_book(
            file,
            &mut corpus,
            &mut catalog,
            &cfg.lancedb_dir(),
            &cfg.books_dir(),
            &embedder,
            &params,
        )
        .await
        {
            Ok(report) => {
                let needs_work_tag = if report.audit_verdict.as_deref() == Some("needs_work") {
                    " \u{26a0} needs_work"
                } else {
                    ""
                };
                if report.no_op {
                    skipped_noop += 1;
                    println!(
                        "  = {} (intake {}, already up to date{needs_work_tag})",
                        file.display(),
                        report.intake_id,
                    );
                } else if report.already_registered {
                    refreshed += 1;
                    let marker = if report.forced {
                        "forced"
                    } else {
                        "stamp drift"
                    };
                    println!(
                        "  ~ {} (intake {}, refreshed [{marker}], {} chunks{needs_work_tag})",
                        file.display(),
                        report.intake_id,
                        report.chunks_written,
                    );
                } else {
                    newly_ingested += 1;
                    println!(
                        "  + {} (intake {}, {} chunks{needs_work_tag})",
                        file.display(),
                        report.intake_id,
                        report.chunks_written,
                    );
                }
            }
            Err(e) => {
                let message = format!("{e:#}");
                tracing::warn!(
                    file = %file.display(),
                    error = %message,
                    "ingest failed; continuing",
                );
                println!("  ! {} — failed: {message}", file.display());
                failed.push((file.clone(), message));
            }
        }
    }
    println!();
    println!(
        "Recursive ingest summary: {newly_ingested} new, {refreshed} refreshed, \
         {skipped_noop} already up to date, {} failed.",
        failed.len(),
    );
    if skipped_noop > 0 && !force {
        println!("  (Pass --force to re-extract and re-embed up-to-date intakes.)");
    }
    if !failed.is_empty() {
        anyhow::bail!("{} file(s) failed during recursive ingest", failed.len());
    }
    Ok(())
}

/// Walk `dir` depth-first and collect every regular file whose extension
/// is one of the formats `bookrack ingest` supports. Hidden files (those
/// whose name starts with `.`) are skipped.
fn collect_supported_files(dir: &Path) -> Result<Vec<PathBuf>> {
    const SUPPORTED: &[&str] = &["epub", "pdf", "mobi", "azw3", "txt"];
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .with_context(|| format!("read_dir {}", current.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("entry of {}", current.display()))?;
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let metadata = entry
                .metadata()
                .with_context(|| format!("metadata of {}", path.display()))?;
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            if let Some(ext) = ext
                && SUPPORTED.contains(&ext.as_str())
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

async fn run_query(
    cfg: &Config,
    text: &str,
    in_book: Option<i64>,
    bypass_ann: bool,
    nprobes: Option<usize>,
    refine_factor: Option<u32>,
) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let search_cfg = SearchConfig::from_env();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    // The catalog handle is opened beside the corpus so the breadcrumb
    // resolver can read the effective book title; it is used only
    // synchronously for citation and dropped at the end of this scope.
    let catalog = Catalog::open(&cfg.catalog_db()).context("open catalog")?;
    // Refuse a `--in-book` against an unknown or already-removed intake
    // up front, before the embedder probe and the vector store open.
    // Without this guard the query silently returns zero hits and reads
    // as "this book is fine, it just has no matches" — which is the
    // opposite of what happened.
    if let Some(intake_id) = in_book
        && catalog
            .intake_by_id(intake_id)
            .context("look up intake")?
            .is_none()
    {
        anyhow::bail!("no intake registered for book {intake_id}");
    }
    let embedder = embedder(cfg, &embed_cfg)?;

    // The store's vector width is fixed at creation and must match the
    // model. Probe the embedder once to learn it before reopening.
    let probe = embedder
        .embed_batch(&["dimension probe".to_string()])
        .await
        .context("probe embedding dimension")?;
    let dim = probe
        .first()
        .map(Vec::len)
        .context("embedder returned no vector")?;

    let store = ChunkStore::open(&cfg.lancedb_dir(), dim)
        .await
        .context("open vector store")?;
    // Refuse to serve an index built with a different model or a stale
    // algorithm version; an empty index has no provenance to check.
    if store.count_rows().await.context("count vector rows")? > 0 {
        corpus
            .verify_index_stamps(&bookrack_ingest::current_index_stamps(
                &embed_cfg.model,
                dim as u32,
            ))
            .context("verify index stamps")?;
    }
    // CLI flags win over env, which wins over meta defaults inside
    // retrieve_with.
    let env = bookrack_search::env_overrides();
    let overrides = bookrack_vectors::SearchOptions {
        bypass_index: bypass_ann || env.bypass_index,
        nprobes: nprobes.or(env.nprobes),
        refine_factor: refine_factor.or(env.refine_factor),
    };
    let hits = if let Some(intake_id) = in_book {
        let partition = PartitionIdx::new(intake_id);
        let raw = bookrack_search::retrieve_with_partition(
            text,
            &store,
            &embedder,
            &cfg.lancedb_dir(),
            overrides,
            search_cfg.top_k,
            partition,
        )
        .await
        .context("run query in book")?;
        bookrack_search::cite(&corpus, &catalog, raw).context("cite passages")?
    } else {
        bookrack_search::search_with(
            text,
            &corpus,
            &catalog,
            &store,
            &embedder,
            &cfg.lancedb_dir(),
            overrides,
            search_cfg.top_k,
        )
        .await
        .context("run query")?
    };
    render::citations(&hits, search_cfg.weak_distance_threshold);
    Ok(())
}

/// Logical address of the book root; the CLI's metadata commands only
/// touch this scope, matching the audit and the ingest sub-step.
const BOOK_SCOPE: &str = "book";

async fn run_metadata(
    cfg: &Config,
    action: MetadataAction,
    profile_name: Option<&str>,
) -> Result<()> {
    // Advance opens its own corpus + catalog + embedder, since it
    // runs CHUNK→EMBED rather than touching catalog alone. The
    // other actions only need catalog and can share this handle.
    if let MetadataAction::Advance { book } = action {
        return run_metadata_advance(cfg, book, profile_name).await;
    }
    // The audit-profile reflection commands need no catalog and no audit
    // rules, so they short-circuit before the catalog open.
    if let MetadataAction::AuditProfile { action } = action {
        return run_metadata_audit_profile(action);
    }
    // Trigger any pending catalog migration (with a pre-migration
    // backup snapshot) once before dispatching. The write ops below
    // open their own per-call handles via ops, which only see the
    // already-migrated database.
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let audit_rules = load_audit_rules(cfg);
    let audit_profile = load_audit_profile(cfg, profile_name);
    let ops = catalog_only_ops(cfg);
    match action {
        MetadataAction::Show { book, json } => {
            run_metadata_show(&catalog, book, json, &audit_rules, &audit_profile)
        }
        MetadataAction::Set { book, field, value } => run_metadata_set(&ops, book, &field, &value),
        MetadataAction::Clear { book, field } => run_metadata_clear(&ops, book, &field),
        MetadataAction::Ack { book, reason } => run_metadata_ack(&ops, book, &reason),
        MetadataAction::Approve { book, reason } => {
            run_metadata_approve(&ops, book, reason.as_deref())
        }
        MetadataAction::Reject { book, reason } => run_metadata_reject(&ops, book, &reason),
        MetadataAction::List {
            needs_review,
            limit,
            offset,
            json,
        } => run_metadata_list(&catalog, needs_review, limit, offset, json),
        MetadataAction::AuditTrail { book, json } => run_metadata_audit_trail(&catalog, book, json),
        MetadataAction::Advance { .. } => unreachable!("handled above"),
        MetadataAction::AuditProfile { .. } => unreachable!("handled above"),
    }
}

fn run_metadata_audit_profile(action: AuditProfileAction) -> Result<()> {
    match action {
        AuditProfileAction::List { json } => {
            if json {
                render::audit_profile_names_json(bookrack_audit_profile::ALL_BUILT_IN_NAMES);
            } else {
                for name in bookrack_audit_profile::ALL_BUILT_IN_NAMES {
                    println!("{name}");
                }
            }
            Ok(())
        }
        AuditProfileAction::Show { name } => {
            let profile = bookrack_audit_profile::AuditProfile::from_named(&name)
                .with_context(|| format!("unknown profile {name:?}"))?;
            render::audit_profile_show(&name, &profile);
            Ok(())
        }
        AuditProfileAction::Diff { a, b } => {
            let pa = bookrack_audit_profile::AuditProfile::from_named(&a)
                .with_context(|| format!("unknown profile {a:?}"))?;
            let pb = bookrack_audit_profile::AuditProfile::from_named(&b)
                .with_context(|| format!("unknown profile {b:?}"))?;
            render::audit_profile_diff(&a, &pa, &b, &pb);
            Ok(())
        }
    }
}

fn run_metadata_list(
    catalog: &Catalog,
    needs_review: bool,
    limit: u32,
    offset: u32,
    json: bool,
) -> Result<()> {
    let needs_review_confidence: &[&str] = &["low", "medium"];
    let needs_review_status: &[&str] = &[STATUS_PENDING, STATUS_ACKNOWLEDGED];
    let filter = if needs_review {
        IntakeFilter {
            confidence_in: needs_review_confidence,
            review_status_in: needs_review_status,
            ..IntakeFilter::default()
        }
    } else {
        IntakeFilter::default()
    };
    let intakes = catalog
        .find_intakes(&filter, limit, offset)
        .context("find intakes")?;
    let total = catalog
        .count_find_intakes(&filter)
        .context("count intakes")?;
    let mut rows = Vec::with_capacity(intakes.len());
    for intake in intakes {
        let effective = catalog
            .effective_publication_attrs(intake.intake_id, BOOK_SCOPE)
            .context("read effective metadata")?;
        let title = effective.get("title").map(str::to_string);
        let attrs = catalog
            .publication_attrs(intake.intake_id, BOOK_SCOPE)
            .context("read publication attrs")?;
        let confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
        let review = catalog
            .review(intake.intake_id, BOOK_SCOPE)
            .context("read review")?
            .map(|r| r.status);
        rows.push(render::MetadataListRow {
            intake_id: intake.intake_id,
            title,
            confidence,
            review_status: review,
        });
    }
    if json {
        render::metadata_list_json(&rows, total);
    } else {
        render::metadata_list(&rows, total, needs_review);
    }
    Ok(())
}

fn run_metadata_audit_trail(catalog: &Catalog, book: i64, json: bool) -> Result<()> {
    // `metadata_audit` rows outlive the `intake` row by design — see
    // the matching contract in `bookrack_ops::reads::metadata`. Read
    // first, then only consult `intake_by_id` to disambiguate the
    // empty result.
    let node_id = PartitionIdx::new(book).root().get();
    let rows = catalog
        .metadata_audit_for_node(node_id)
        .context("read metadata audit")?;
    if rows.is_empty()
        && catalog
            .intake_by_id(book)
            .context("look up intake")?
            .is_none()
    {
        anyhow::bail!("no intake registered for book {book}");
    }
    if json {
        render::metadata_audit_trail_json(book, &rows);
    } else {
        render::metadata_audit_trail(book, &rows);
    }
    Ok(())
}

fn run_pipeline_trail(cfg: &Config, book: i64, json: bool) -> Result<()> {
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    // `book_pipeline_audit` rows outlive the `intake` row by design —
    // see the matching contract in `bookrack_ops::reads::pipeline`.
    // Read first, then only consult `intake_by_id` to disambiguate
    // the empty result.
    let book_root_id = PartitionIdx::new(book).root().get();
    let rows = catalog
        .pipeline_audit_for_book(book_root_id)
        .context("read pipeline audit")?;
    if rows.is_empty()
        && catalog
            .intake_by_id(book)
            .context("look up intake")?
            .is_none()
    {
        anyhow::bail!("no intake registered for book {book}");
    }
    if json {
        render::pipeline_trail_json(book, &rows);
    } else {
        render::pipeline_trail(book, &rows);
    }
    Ok(())
}

fn run_metadata_show(
    catalog: &Catalog,
    book: i64,
    json: bool,
    rules: &AuditRules,
    profile: &bookrack_metadata::AuditProfile,
) -> Result<()> {
    catalog
        .intake_by_id(book)
        .context("look up intake")?
        .with_context(|| format!("no intake registered for book {book}"))?;
    let effective = catalog
        .effective_publication_attrs(book, BOOK_SCOPE)
        .context("read effective metadata")?;
    let attrs = catalog
        .publication_attrs(book, BOOK_SCOPE)
        .context("read publication attrs")?;
    let review_status = catalog
        .review(book, BOOK_SCOPE)
        .context("read review row")?
        .map(|r| r.status);
    // The audit needs an adapter so its source-format prior can fire;
    // reuse the one stamped on the base row at ingest time, falling
    // back to a neutral marker when the row has not been written yet.
    let adapter = attrs
        .as_ref()
        .and_then(|a| a.source_format.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let biblio = Biblio::default();
    let provenance = Provenance {
        adapter,
        // 0 marks a synthesized provenance: no extractor produced this,
        // so any current `EXTRACTOR_VERSION` will compare as mismatched.
        extractor_version: 0,
        text_layer_quality: TextLayerQuality::BornDigital,
        skipped_units: Vec::new(),
        derived_from_sha256: None,
        partial_pages: None,
    };
    let toc_stats = TocStats::default();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &provenance,
        effective: &effective,
        toc_stats: &toc_stats,
        body_sample: "",
        total_blocks: 0,
        source_stem: None,
        rules,
    };
    let report = bookrack_metadata::audit(&input, profile);
    let stored_verdict = attrs.as_ref().and_then(|a| a.audit_verdict.clone());
    let stored_confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
    if json {
        render::metadata_show_json(
            book,
            &report,
            review_status.as_deref(),
            stored_verdict.as_deref(),
            stored_confidence.as_deref(),
        );
    } else {
        render::metadata_show(
            book,
            &report,
            review_status.as_deref(),
            stored_verdict.as_deref(),
            stored_confidence.as_deref(),
        );
    }
    Ok(())
}

fn run_metadata_set(
    ops: &Ops<OllamaEmbedClient>,
    book: i64,
    field: &str,
    value: &str,
) -> Result<()> {
    let req = bookrack_ops::dto::writes::SetMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        value: value.to_string(),
    };
    match bookrack_ops::writes::metadata::set_metadata_field(ops, req) {
        Ok(_) => {
            println!("Set {field} on book {book}.");
            Ok(())
        }
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("set metadata field via ops")),
    }
}

fn run_metadata_clear(ops: &Ops<OllamaEmbedClient>, book: i64, field: &str) -> Result<()> {
    let req = bookrack_ops::dto::writes::ClearMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
    };
    match bookrack_ops::writes::metadata::clear_metadata_field(ops, req) {
        Ok(outcome) => {
            if outcome.changed {
                println!("Cleared override on {field} for book {book}.");
            } else {
                println!("No override on {field} for book {book}; nothing to clear.");
            }
            Ok(())
        }
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("clear metadata field via ops")),
    }
}

fn run_metadata_ack(ops: &Ops<OllamaEmbedClient>, book: i64, reason: &str) -> Result<()> {
    let req = bookrack_ops::dto::writes::AcknowledgeMetadataGapRequest {
        intake_id: book,
        reason: reason.to_string(),
    };
    match bookrack_ops::writes::metadata::acknowledge_metadata_gap(ops, req) {
        Ok(_) => {
            println!("Acknowledged metadata gap on book {book}.");
            Ok(())
        }
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("acknowledge metadata gap via ops")),
    }
}

/// Mark the record reviewed and correct. The operator (or an LLM acting
/// on the operator's behalf) is asserting that the effective metadata
/// matches the source; the audit's plausibility verdict is unchanged.
fn run_metadata_approve(
    ops: &Ops<OllamaEmbedClient>,
    book: i64,
    reason: Option<&str>,
) -> Result<()> {
    let req = bookrack_ops::dto::writes::ApproveMetadataRequest {
        intake_id: book,
        reason: reason.map(str::to_string),
    };
    match bookrack_ops::writes::metadata::approve_metadata(ops, req) {
        Ok(_) => {
            println!("Approved metadata on book {book}.");
            Ok(())
        }
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("approve metadata via ops")),
    }
}

/// Reject the book. The pipeline rows stay in place so downstream
/// consumers can filter on `rejected`; this records the rejection and
/// the reason in the audit trail.
fn run_metadata_reject(ops: &Ops<OllamaEmbedClient>, book: i64, reason: &str) -> Result<()> {
    let req = bookrack_ops::dto::writes::RejectMetadataRequest {
        intake_id: book,
        reason: reason.to_string(),
    };
    match bookrack_ops::writes::metadata::reject_metadata(ops, req) {
        Ok(_) => {
            println!("Rejected book {book}.");
            Ok(())
        }
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("reject metadata via ops")),
    }
}

async fn run_metadata_advance(cfg: &Config, book: i64, profile_name: Option<&str>) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let audit_profile = load_audit_profile(cfg, profile_name);

    let book_root_id = PartitionIdx::new(book).root();
    let intake = catalog
        .intake_by_id(book)
        .context("look up intake")?
        .with_context(|| format!("no intake registered for book {book}"))?;
    let state = catalog
        .book_state(book_root_id.get())
        .context("read book state")?
        .with_context(|| format!("no book state for book {book}"))?;
    let parsed_at = state
        .parsed_at
        .clone()
        .with_context(|| format!("book {book} has no parsed_at; STRUCTURE has not run"))?;
    // Mint a fresh run id so resume rows are distinguishable from the
    // original ingest's; pin them to the same source_sha for traceability.
    let run_id = format!(
        "advance-{}-{book}",
        &intake.source_sha256[..8.min(intake.source_sha256.len())]
    );
    let params = IngestParams {
        embed: embed_cfg,
        audit_profile,
        ..Default::default()
    };
    let embedder = embedder(cfg, &params.embed)?;

    let report = resume_from_chunk(
        &mut corpus,
        &mut catalog,
        &cfg.lancedb_dir(),
        &embedder,
        &params,
        book,
        book_root_id,
        &run_id,
        &intake.source_sha256,
        &parsed_at,
    )
    .await
    .context("resume CHUNK→EMBED")?;
    println!(
        "Advanced book {book}: embedded {} chunks across {} batches.",
        report.chunks_written, report.batches
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn data_dir_and_library_are_mutually_exclusive() {
        let parsed = Cli::try_parse_from([
            "bookrack",
            "--data-dir",
            "/x",
            "--library",
            "test",
            "query",
            "q",
        ]);
        assert!(parsed.is_err(), "the two selectors must not be combined");
    }

    #[test]
    fn selection_carries_the_flags_through() {
        let cli = Cli::try_parse_from(["bookrack", "--library", "test", "query", "q"])
            .expect("a lone --library parses");
        let selection = cli.selection();
        assert_eq!(selection.library.as_deref(), Some("test"));
        assert!(selection.data_dir.is_none());
    }

    #[test]
    fn metadata_subcommands_parse() {
        for argv in [
            vec!["bookrack", "metadata", "show", "1"],
            vec!["bookrack", "metadata", "show", "1", "--json"],
            vec!["bookrack", "metadata", "set", "1", "title", "A New Title"],
            vec!["bookrack", "metadata", "set", "1", "pub_place", "New York"],
            vec!["bookrack", "metadata", "set", "1", "original_year", "1949"],
            vec!["bookrack", "metadata", "clear", "1", "title"],
            vec!["bookrack", "metadata", "ack", "1", "--reason", "test"],
            vec!["bookrack", "metadata", "approve", "1"],
            vec![
                "bookrack", "metadata", "approve", "1", "--reason", "verified",
            ],
            vec![
                "bookrack",
                "metadata",
                "reject",
                "1",
                "--reason",
                "wrong file",
            ],
            vec!["bookrack", "metadata", "advance", "1"],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    #[test]
    fn ingest_accepts_hold_for_metadata_flag() {
        Cli::try_parse_from(["bookrack", "ingest", "/x/book.epub", "--hold-for-metadata"])
            .expect("the flag parses");
    }

    #[test]
    fn dryrun_subcommand_parses() {
        for argv in [
            vec!["bookrack", "dryrun", "/x"],
            vec!["bookrack", "dryrun", "/x", "--stdout"],
            vec!["bookrack", "dryrun", "/x", "--no-chunk"],
            vec!["bookrack", "dryrun", "/x", "--out", "/tmp/r.jsonl"],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    // The behavioural coverage of `run_metadata_*` lives in
    // `crates/ops/tests/metadata_writes.rs`, where the logic itself sits.
    // The CLI handlers here are thin shells that hand the request off to
    // `bookrack_ops::writes::metadata::*`.

    #[test]
    fn natural_name_hints_cover_the_common_typos_from_the_test_report() {
        for (typed, expected) in [
            ("list", "`bookrack books list`"),
            ("ls", "`bookrack books list`"),
            ("find", "`bookrack books find <text>`"),
            ("show", "`bookrack books show <id>`"),
            ("stats", "`bookrack books stats`"),
            ("search", "`bookrack query <text>`"),
        ] {
            assert_eq!(natural_name_hint(typed).as_deref(), Some(expected));
        }

        // `status` is ambiguous between library-level and per-book; the
        // hint surfaces both so the user picks.
        let status = natural_name_hint("status").expect("status maps");
        assert!(status.contains("`bookrack info`"));
        assert!(status.contains("`bookrack books stats`"));
        assert!(status.contains(" or "));

        // Tokens not in the table fall through to clap's similarity tip;
        // returning None is how we signal that.
        assert_eq!(natural_name_hint("nope"), None);
        assert_eq!(natural_name_hint(""), None);
    }

    #[test]
    fn remove_subcommand_parses_both_input_shapes() {
        // Positional intake id, --sha alternative, and the destructive
        // toggles must all parse without --library or --data-dir.
        for argv in [
            vec!["bookrack", "remove", "42"],
            vec!["bookrack", "remove", "42", "--dry-run"],
            vec!["bookrack", "remove", "42", "--yes"],
            vec!["bookrack", "remove", "--sha", "deadbeef"],
            vec!["bookrack", "remove", "--sha", "deadbeef", "--dry-run"],
        ] {
            Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    #[test]
    fn verify_short_circuits_on_an_uninitialised_data_dir() {
        // A freshly mkdir'd data directory has no catalog.db on disk
        // yet. Verify must NOT try to open one (that would create an
        // empty file and then fail schema verification), and must NOT
        // open corpus.db either (that would write tables into a store
        // verify is supposed to only read).
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(tmp.path().to_path_buf(), "http://localhost:0".to_string());
        let report = build_verify_report(&cfg);
        assert!(report.not_initialised);
        assert!(!report.catalog_schema_ok);
        assert!(report.catalog_schema_error.is_none());
        assert!(!report.corpus_schema_ok);
        assert!(report.corpus_schema_error.is_none());
        assert!(report.vectors_built_at_chunk_count.is_none());
        // The data dir is unchanged — no stores were created as a
        // side effect of the verify call.
        assert!(!cfg.catalog_db().exists());
        assert!(!cfg.corpus_db().exists());
    }

    #[test]
    fn remove_rejects_both_intake_id_and_sha_together() {
        // The `--sha` and positional id select the same target two
        // different ways; supplying both is a user error.
        let Err(err) = Cli::try_parse_from(["bookrack", "remove", "42", "--sha", "abc"]) else {
            panic!("the two selectors must not be combined");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn invalid_subcommand_token_extracts_the_offending_string() {
        let Err(err) = Cli::try_parse_from(["bookrack", "list"]) else {
            panic!("`list` is not a valid subcommand and must error");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert_eq!(invalid_subcommand_token(&err).as_deref(), Some("list"));
    }
}
