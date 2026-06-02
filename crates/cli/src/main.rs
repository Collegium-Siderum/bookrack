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
use bookrack_catalog::{ActorKind, Catalog, NewMetadataAudit, NewOverride, NewReview};
use bookrack_config::{Config, EmbedConfig, LibrarySelection, LogConfig, SearchConfig};
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_extract::{Biblio, Provenance, TextLayerQuality};
use bookrack_ingest::{IngestParams, ingest_book, resume_from_chunk};
use bookrack_metadata::{AuditInput, AuditRules, TocStats};
use bookrack_search::search;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    let cfg = Config::resolve(&cli.selection()).context("resolve configuration")?;
    let _guard = bookrack_obs::init(&cfg, &LogConfig::from_env());

    match cli.command {
        Command::Ingest {
            path,
            hold_for_metadata,
        } => run_ingest(&cfg, &path, hold_for_metadata).await,
        Command::Query { text } => run_query(&cfg, &text).await,
        Command::Metadata { action } => run_metadata(&cfg, action).await,
        Command::Dryrun {
            path,
            out,
            stdout,
            no_chunk,
        } => dryrun::run(&cfg, &path, out.as_deref(), stdout, no_chunk),
    }
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

async fn run_ingest(cfg: &Config, path: &Path, hold_for_metadata: bool) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let embedder = embedder(cfg, &embed_cfg)?;
    let audit_rules = load_audit_rules(cfg);
    let params = IngestParams {
        embed: embed_cfg,
        hold_for_metadata,
        audit_rules,
        ..Default::default()
    };
    let report = ingest_book(
        path,
        &mut corpus,
        &mut catalog,
        &cfg.lancedb_dir(),
        &embedder,
        &params,
    )
    .await
    .context("ingest book")?;
    render::ingest(&report);
    Ok(())
}

async fn run_query(cfg: &Config, text: &str) -> Result<()> {
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
    let hits = search(text, &corpus, &catalog, &store, &embedder, search_cfg.top_k)
        .await
        .context("run query")?;
    render::citations(&hits);
    Ok(())
}

/// Logical address of the book root; the CLI's metadata commands only
/// touch this scope, matching the audit and the ingest sub-step.
const BOOK_SCOPE: &str = "book";

async fn run_metadata(cfg: &Config, action: MetadataAction) -> Result<()> {
    // Advance opens its own corpus + catalog + embedder, since it
    // runs CHUNK→EMBED rather than touching catalog alone. The
    // other actions only need catalog and can share this handle.
    if let MetadataAction::Advance { book } = action {
        return run_metadata_advance(cfg, book).await;
    }
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let audit_rules = load_audit_rules(cfg);
    match action {
        MetadataAction::Show { book, json } => {
            run_metadata_show(&catalog, book, json, &audit_rules)
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
        MetadataAction::Advance { .. } => unreachable!("handled above"),
    }
}

fn run_metadata_show(catalog: &Catalog, book: i64, json: bool, rules: &AuditRules) -> Result<()> {
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
        extractor_version: String::new(),
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
    let report = bookrack_metadata::audit(&input);
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

async fn run_metadata_advance(cfg: &Config, book: i64) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;

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
