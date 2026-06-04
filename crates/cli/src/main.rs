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
mod render;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bookrack_catalog::{
    ActorKind, Catalog, IntakeFilter, NewMetadataAudit, NewOverride, NewReview,
    STATUS_ACKNOWLEDGED, STATUS_PENDING,
};
use bookrack_config::{Config, EmbedConfig, LibrarySelection, LogConfig, SearchConfig};
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_extract::{Biblio, Provenance, TextLayerQuality};
use bookrack_ingest::{IngestParams, ingest_book, resume_from_chunk};
use bookrack_metadata::{AuditInput, AuditRules, TocStats};
use bookrack_vectors::ChunkStore;

#[derive(clap::Parser)]
#[command(name = "bookrack", version, about = "Search a local library of books.")]
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
    /// Ingest and embed a single file into the library.
    Ingest {
        /// Path to the source file to ingest.
        path: PathBuf,
        /// Stop in the metadata stage when the audit verdict is
        /// `needs_work` and wait for an operator. Off by default —
        /// EMBED runs straight through and the audit verdict is
        /// merely advisory. With the flag on, the held book resumes
        /// through `bookrack metadata advance <book>` once an
        /// operator has corrected the record.
        #[arg(long)]
        hold_for_metadata: bool,
    },
    /// Query the library and print cited passages.
    Query {
        /// The natural-language query.
        text: String,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    let cfg = Config::resolve(&cli.selection()).context("resolve configuration")?;
    let _guard = bookrack_obs::init(&cfg, &LogConfig::from_env());

    let profile_name = cli.audit_profile.clone();
    match cli.command {
        Command::Ingest {
            path,
            hold_for_metadata,
        } => run_ingest(&cfg, &path, hold_for_metadata, profile_name.as_deref()).await,
        Command::Query {
            text,
            bypass_ann,
            nprobes,
            refine_factor,
        } => run_query(&cfg, &text, bypass_ann, nprobes, refine_factor).await,
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
    }
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
    if indices.is_empty() {
        println!("ann index:       (none — brute-force)");
    } else {
        for name in indices {
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
    let plans = bookrack_ingest::reembed::plan_reembed(&catalog, &lancedb_dir, book)
        .await
        .context("plan reembed")?;
    if plans.is_empty() {
        println!("no embedded intakes carry chunks; nothing to reembed");
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
    hold_for_metadata: bool,
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
        audit_rules,
        audit_profile,
        ..Default::default()
    };
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
    Ok(())
}

async fn run_query(
    cfg: &Config,
    text: &str,
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
    let hits = bookrack_search::search_with(
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
    .context("run query")?;
    render::citations(&hits);
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
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let audit_rules = load_audit_rules(cfg);
    let audit_profile = load_audit_profile(cfg, profile_name);
    match action {
        MetadataAction::Show { book, json } => {
            run_metadata_show(&catalog, book, json, &audit_rules, &audit_profile)
        }
        MetadataAction::Set { book, field, value } => {
            run_metadata_set(&catalog, book, &field, &value)
        }
        MetadataAction::Clear { book, field } => run_metadata_clear(&catalog, book, &field),
        MetadataAction::Ack { book, reason } => run_metadata_ack(&catalog, book, &reason),
        MetadataAction::Approve { book, reason } => {
            run_metadata_approve(&catalog, book, reason.as_deref())
        }
        MetadataAction::Reject { book, reason } => run_metadata_reject(&catalog, book, &reason),
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
    let node_id = PartitionIdx::new(book).root().get();
    let rows = catalog
        .metadata_audit_for_node(node_id)
        .context("read metadata audit")?;
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
    let book_root_id = PartitionIdx::new(book).root().get();
    let rows = catalog
        .pipeline_audit_for_book(book_root_id)
        .context("read pipeline audit")?;
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
    if json {
        render::metadata_show_json(book, &report, review_status.as_deref());
    } else {
        render::metadata_show(book, &report, review_status.as_deref());
    }
    Ok(())
}

fn run_metadata_set(catalog: &Catalog, book: i64, field: &str, value: &str) -> Result<()> {
    let effective = catalog
        .effective_publication_attrs(book, BOOK_SCOPE)
        .context("read effective metadata")?;
    let old_value = effective.get(field).map(str::to_string);
    catalog
        .set_override(&NewOverride::new(
            book,
            BOOK_SCOPE,
            field,
            Some(value.to_string()),
            "human",
        ))
        .context("write override")?;
    let mut audit = NewMetadataAudit::new("node_publication_attrs", "update", ActorKind::Human);
    audit.node_id = Some(PartitionIdx::new(book).root().get());
    audit.field = Some(field.to_string());
    audit.old_value = old_value;
    audit.new_value = Some(value.to_string());
    catalog
        .record_metadata_audit(&audit)
        .context("record metadata audit")?;
    println!("Set {field} on book {book}.");
    Ok(())
}

fn run_metadata_clear(catalog: &Catalog, book: i64, field: &str) -> Result<()> {
    let effective = catalog
        .effective_publication_attrs(book, BOOK_SCOPE)
        .context("read effective metadata")?;
    let old_value = effective.get(field).map(str::to_string);
    let existed = catalog
        .clear_override(book, BOOK_SCOPE, field)
        .context("clear override")?;
    if !existed {
        println!("No override on {field} for book {book}; nothing to clear.");
        return Ok(());
    }
    let mut audit = NewMetadataAudit::new("node_publication_attrs", "delete", ActorKind::Human);
    audit.node_id = Some(PartitionIdx::new(book).root().get());
    audit.field = Some(field.to_string());
    audit.old_value = old_value;
    catalog
        .record_metadata_audit(&audit)
        .context("record metadata audit")?;
    println!("Cleared override on {field} for book {book}.");
    Ok(())
}

fn run_metadata_ack(catalog: &Catalog, book: i64, reason: &str) -> Result<()> {
    let mut audit = NewMetadataAudit::new("node_reviews", "acknowledge_gate", ActorKind::Human);
    audit.node_id = Some(PartitionIdx::new(book).root().get());
    audit.reason = Some(reason.to_string());
    catalog
        .record_metadata_audit(&audit)
        .context("record metadata audit")?;
    catalog
        .upsert_review(&NewReview::new(
            book,
            BOOK_SCOPE,
            "human",
            bookrack_catalog::STATUS_ACKNOWLEDGED,
        ))
        .context("upsert review")?;
    println!("Acknowledged metadata gap on book {book}.");
    Ok(())
}

/// Mark the record reviewed and correct. The operator (or an LLM acting
/// on the operator's behalf) is asserting that the effective metadata
/// matches the source; the audit's plausibility verdict is unchanged.
fn run_metadata_approve(catalog: &Catalog, book: i64, reason: Option<&str>) -> Result<()> {
    let mut audit = NewMetadataAudit::new("node_reviews", "approve", ActorKind::Human);
    audit.node_id = Some(PartitionIdx::new(book).root().get());
    audit.reason = reason.map(str::to_string);
    catalog
        .record_metadata_audit(&audit)
        .context("record metadata audit")?;
    let mut review = NewReview::new(book, BOOK_SCOPE, "human", bookrack_catalog::STATUS_APPROVED);
    if let Some(r) = reason {
        review = review.notes(r);
    }
    catalog.upsert_review(&review).context("upsert review")?;
    println!("Approved metadata on book {book}.");
    Ok(())
}

/// Reject the book. The pipeline rows stay in place so downstream
/// consumers can filter on `rejected`; this records the human's
/// rejection and the reason in the audit trail.
fn run_metadata_reject(catalog: &Catalog, book: i64, reason: &str) -> Result<()> {
    let mut audit = NewMetadataAudit::new("node_reviews", "reject", ActorKind::Human);
    audit.node_id = Some(PartitionIdx::new(book).root().get());
    audit.reason = Some(reason.to_string());
    catalog
        .record_metadata_audit(&audit)
        .context("record metadata audit")?;
    catalog
        .upsert_review(
            &NewReview::new(book, BOOK_SCOPE, "human", bookrack_catalog::STATUS_REJECTED)
                .notes(reason),
        )
        .context("upsert review")?;
    println!("Rejected book {book}.");
    Ok(())
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

    #[test]
    fn metadata_set_records_the_override_and_an_update_audit_row() {
        let catalog = Catalog::open_in_memory().expect("open");
        run_metadata_set(&catalog, 7, "title", "A New Title").expect("set");
        let effective = catalog
            .effective_publication_attrs(7, BOOK_SCOPE)
            .expect("effective");
        assert_eq!(effective.get("title"), Some("A New Title"));

        let book_root_id = PartitionIdx::new(7).root().get();
        let audit = catalog
            .metadata_audit_for_node(book_root_id)
            .expect("audit");
        let update_row = audit
            .iter()
            .find(|r| r.action == "update")
            .expect("an update row");
        assert_eq!(update_row.field.as_deref(), Some("title"));
        assert_eq!(update_row.new_value.as_deref(), Some("A New Title"));
        assert!(update_row.old_value.is_none());
    }

    #[test]
    fn metadata_set_pub_place_and_original_year_flow_through_the_effective_view() {
        // The two new schema columns are settable via the EAV override
        // path even without a base row; the effective view returns both.
        let catalog = Catalog::open_in_memory().expect("open");
        run_metadata_set(&catalog, 9, "pub_place", "New York").expect("set pub_place");
        run_metadata_set(&catalog, 9, "original_year", "1949").expect("set original_year");
        let effective = catalog
            .effective_publication_attrs(9, BOOK_SCOPE)
            .expect("effective");
        assert_eq!(effective.get("pub_place"), Some("New York"));
        assert_eq!(effective.get("original_year"), Some("1949"));
    }

    #[test]
    fn metadata_clear_falls_back_to_base_and_records_a_delete() {
        let catalog = Catalog::open_in_memory().expect("open");
        // Seed a base title, then add an override, then clear it.
        let mut base = bookrack_catalog::NewPublicationAttrs::new(7, BOOK_SCOPE);
        base.title = Some("Base Title".to_string());
        catalog.upsert_publication_attrs(&base).expect("base");
        run_metadata_set(&catalog, 7, "title", "Override Title").expect("set");
        run_metadata_clear(&catalog, 7, "title").expect("clear");

        let effective = catalog
            .effective_publication_attrs(7, BOOK_SCOPE)
            .expect("effective");
        // The override is gone, so the base value is what remains.
        assert_eq!(effective.get("title"), Some("Base Title"));

        let book_root_id = PartitionIdx::new(7).root().get();
        let audit = catalog
            .metadata_audit_for_node(book_root_id)
            .expect("audit");
        assert!(audit.iter().any(|r| r.action == "delete"));
    }

    #[test]
    fn metadata_ack_records_a_review_and_a_gate_audit_row() {
        let catalog = Catalog::open_in_memory().expect("open");
        run_metadata_ack(&catalog, 11, "operator vetted").expect("ack");
        let review = catalog
            .review(11, BOOK_SCOPE)
            .expect("review")
            .expect("present");
        assert_eq!(review.status, bookrack_catalog::STATUS_ACKNOWLEDGED);
        let book_root_id = PartitionIdx::new(11).root().get();
        let audit = catalog
            .metadata_audit_for_node(book_root_id)
            .expect("audit");
        assert!(
            audit.iter().any(|r| r.action == "acknowledge_gate"),
            "audit rows: {audit:?}"
        );
    }

    #[test]
    fn metadata_approve_records_a_review_and_an_approval_audit_row() {
        let catalog = Catalog::open_in_memory().expect("open");
        run_metadata_approve(&catalog, 13, Some("checked against the printed copy"))
            .expect("approve");
        let review = catalog
            .review(13, BOOK_SCOPE)
            .expect("review")
            .expect("present");
        assert_eq!(review.status, bookrack_catalog::STATUS_APPROVED);
        assert_eq!(review.reviewed_by, "human");
        let book_root_id = PartitionIdx::new(13).root().get();
        let audit = catalog
            .metadata_audit_for_node(book_root_id)
            .expect("audit");
        assert!(
            audit.iter().any(|r| r.action == "approve"),
            "audit rows: {audit:?}"
        );
    }

    #[test]
    fn metadata_approve_without_a_reason_still_records_the_audit_row() {
        let catalog = Catalog::open_in_memory().expect("open");
        run_metadata_approve(&catalog, 17, None).expect("approve");
        let review = catalog
            .review(17, BOOK_SCOPE)
            .expect("review")
            .expect("present");
        assert_eq!(review.status, bookrack_catalog::STATUS_APPROVED);
        assert_eq!(review.notes, None);
    }

    #[test]
    fn metadata_reject_records_a_review_and_a_reject_audit_row() {
        let catalog = Catalog::open_in_memory().expect("open");
        run_metadata_reject(&catalog, 19, "wrong source file").expect("reject");
        let review = catalog
            .review(19, BOOK_SCOPE)
            .expect("review")
            .expect("present");
        assert_eq!(review.status, bookrack_catalog::STATUS_REJECTED);
        assert_eq!(review.notes.as_deref(), Some("wrong source file"));
        let book_root_id = PartitionIdx::new(19).root().get();
        let audit = catalog
            .metadata_audit_for_node(book_root_id)
            .expect("audit");
        assert!(
            audit.iter().any(|r| r.action == "reject"),
            "audit rows: {audit:?}"
        );
    }
}
