//! Shared `clap` grammar for the in-process REPL fallback inside
//! `bookrack run` and the standalone `bookrack repl` control-socket
//! client. Lives in its own crate so the client process can parse the
//! same command syntax without depending on the daemon stack.

use std::path::PathBuf;

/// Top-level parser for one REPL line. Behaves like a binary with no
/// program name: the first token is the subcommand.
#[derive(clap::Parser, Debug)]
#[command(name = "", no_binary_name = true)]
pub struct ReplCli {
    #[command(subcommand)]
    pub command: ReplCommand,
}

/// Every write command available from a REPL line. Dispatch maps each
/// variant to the matching control-plane RPC (in the standalone client)
/// or the matching `bookrack_runtime::cmd::*` runner (in the in-process
/// fallback path).
#[derive(clap::Subcommand, Debug)]
pub enum ReplCommand {
    /// Ingest and embed a single file (or, with `--recursive`, every
    /// supported file under a directory) into the library. Inside the
    /// REPL this runs synchronously; queue an entire directory through
    /// the queue worker with `queue add <path>` instead.
    Ingest(IngestArgs),
    /// Drive an intake from a derived source manifestation.
    Intake {
        #[command(subcommand)]
        action: IntakeAction,
    },
    /// Edit one book's metadata: set / clear / ack / approve / reject
    /// / advance.
    Metadata {
        #[command(subcommand)]
        action: WriteMetadataAction,
    },
    /// Vector-store writes: ANN rebuild, brute-force drop, re-embed.
    Vectors {
        #[command(subcommand)]
        action: WriteVectorsAction,
    },
    /// Corpus rebuild from the opaque envelopes.
    Corpus {
        #[command(subcommand)]
        action: CorpusAction,
    },
    /// Reconcile `corpus.db` index_meta stamps.
    Stamps {
        #[command(subcommand)]
        action: StampsAction,
    },
    /// Drop a book from every store.
    Remove(RemoveArgs),
    /// Simulate an ingest up to (but not including) embedding, and write
    /// a JSON report of what the metadata audit would have produced. The
    /// real catalog, corpus, and vector store are not touched.
    Dryrun(DryrunArgs),
    /// Queue lifecycle: pause / resume / clear pending rows.
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Paper-side surface: ingest a paper, browse the paper catalog,
    /// export one paper's bibliographic record as CSL-JSON.
    Papers {
        #[command(subcommand)]
        action: PapersAction,
    },
}

/// One of three lifecycle actions on the persistent ingest queue.
/// Dispatch maps each variant to the matching control-plane RPC.
#[derive(clap::Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    /// Pause the worker loop. Running jobs run to completion; pending
    /// rows stay pending until `resume`.
    Pause,
    /// Resume the worker loop, allowing it to pull pending rows again.
    Resume,
    /// Cancel every pending row in one sweep. Running jobs are left
    /// alone.
    Clear,
}

/// Positional + flag bundle for `ingest`. Lives in a standalone struct
/// so the same type can be embedded in `ReplCommand::Ingest` (parsed
/// from inside the REPL) and `Command::Ingest` (parsed from the
/// top-level CLI) without duplicating attributes.
#[derive(clap::Args, Debug, Clone)]
pub struct IngestArgs {
    pub path: PathBuf,
    #[arg(long)]
    pub recursive: bool,
    #[arg(long)]
    pub hold_for_metadata: bool,
    #[arg(long)]
    pub force: bool,
}

/// Positional + flag bundle for `remove`.
#[derive(clap::Args, Debug, Clone)]
pub struct RemoveArgs {
    pub intake_id: Option<i64>,
    #[arg(long, conflicts_with = "intake_id", value_name = "HEX")]
    pub sha: Option<String>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub yes: bool,
}

/// Positional + flag bundle for `dryrun`.
#[derive(clap::Args, Debug, Clone)]
pub struct DryrunArgs {
    /// Source file, or a directory the dryrun walks recursively.
    pub path: PathBuf,
    /// Write the per-book report to this path instead of the default
    /// `<data_root>/dryruns/...` location. Implies the summary is
    /// written alongside with a `.summary.json` suffix.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Write JSONL to stdout instead of to a file. The summary still
    /// lands on stderr at the end of the run.
    #[arg(long)]
    pub stdout: bool,
    /// Skip the CHUNK step. Saves seconds per large book when only
    /// the audit verdict is wanted.
    #[arg(long)]
    pub no_chunk: bool,
}

/// Intake-side write commands (currently OCR-only).
#[derive(clap::Subcommand, Debug)]
pub enum IntakeAction {
    /// Bring an OCR product into the library as a derived source
    /// manifestation. The scan PDF named by `--from-pdf` is registered
    /// as the durable source anchor (status `needs_ocr`); the OCR
    /// markdown is registered as its own intake whose `Provenance`
    /// forensically references the PDF's hash and flows through
    /// STRUCTURE / CHUNK / EMBED. The expected page count comes from
    /// the source PDF's `/Pages`; pass `--expected-pages` to override
    /// it when PDFium cannot read the source, and `--allow-partial`
    /// to accept an OCR product whose sheets do not cover every page.
    Ocr {
        /// Path to the polyocr single-file Markdown product, with
        /// page markers `<!-- page <label> (sheet <n>) -->`.
        ocr_md: PathBuf,
        /// Path to the scan PDF the OCR product was produced from.
        #[arg(long, value_name = "PDF")]
        from_pdf: PathBuf,
        /// Override the expected page count rather than reading it
        /// from the source PDF's `/Pages`.
        #[arg(long, value_name = "N")]
        expected_pages: Option<u32>,
        /// Accept a partial OCR product. The present sheets are
        /// recorded into `Provenance.partial_pages`; missing pages
        /// surface in the OCR intake's `partial_pages` field rather
        /// than being silently treated as blank.
        #[arg(long)]
        allow_partial: bool,
    },
}

/// Metadata-side write commands. Lives in the grammar crate so both
/// the daemon-side runner (`bookrack_runtime::cmd::metadata::run_write`)
/// and the REPL client can parse them.
#[derive(clap::Subcommand, Debug)]
pub enum WriteMetadataAction {
    /// Set (or change) one metadata field's value.
    Set {
        /// The intake id of the book.
        book: i64,
        /// The field column on `node_publication_attrs` to write
        /// (e.g. `title`, `publisher`, `year`, `language`).
        field: String,
        /// The new value.
        value: String,
        /// Optional note on why this value is correct, recorded on the
        /// audit row.
        #[arg(long)]
        reason: Option<String>,
        /// Mark the override confirmed: the curator has checked the
        /// value against the source itself (e.g. the copyright page).
        /// The audit grades a confirmed override strong unless a
        /// validation check fails.
        #[arg(long)]
        confirmed: bool,
    },
    /// Clear an override, falling back to the extracted base value.
    Clear {
        /// The intake id of the book.
        book: i64,
        /// The field whose override is removed.
        field: String,
        /// Optional note on why the override is removed, recorded on
        /// the audit row.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Suppress a field's extracted value without supplying a
    /// replacement: the field reads as absent until a correct value is
    /// set. `clear` removes the suppression.
    Void {
        /// The intake id of the book.
        book: i64,
        /// The field whose extracted value is suppressed.
        field: String,
        /// Optional note on why the extracted value is wrong, recorded
        /// on the audit row.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Re-run the metadata plausibility audit from the book's cached
    /// extraction, refreshing the stored verdict / confidence so they
    /// reflect the current effective metadata. The review status is
    /// untouched.
    Reaudit {
        /// The intake id of the book.
        book: i64,
    },
    /// Attribute a contributor to the book (origin `user`), appended
    /// after the role's existing contributors. User rows survive a
    /// re-ingest.
    ContributorAdd {
        /// The intake id of the book.
        book: i64,
        /// Contribution role: author / translator / editor / other.
        role: String,
        /// The contributor's name.
        name: String,
        /// The contributor's nationality, when known.
        #[arg(long)]
        nationality: Option<String>,
        /// Optional note on why this attribution is correct, recorded
        /// on the audit row.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Remove one contributor row by its surrogate id (listed by
    /// `show_book`), whatever its origin — the path for stripping a
    /// wrong extracted attribution.
    ContributorRemove {
        /// The intake id of the book.
        book: i64,
        /// The contributor row's surrogate id.
        contributor_id: i64,
        /// Optional note on why the attribution is removed, recorded
        /// on the audit row.
        #[arg(long)]
        reason: Option<String>,
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

/// Vector-store write commands.
#[derive(clap::Subcommand, Debug)]
pub enum WriteVectorsAction {
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
    /// new vectors. Use when the chunking or normalization algorithm
    /// bumped; for an embedding model swap use `libraries fork` or
    /// `vectors reset`.
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
    /// Drop the chunks table, clear the corpus index stamps, and
    /// re-derive every book's vectors with the env-configured
    /// embedding model. Use after switching `BOOKRACK_EMBED_MODEL`.
    /// The old vectors are unrecoverable; consider `libraries fork`
    /// for a non-destructive trial first.
    Reset {
        /// Skip the destructive-action confirmation prompt. The
        /// command still rejects the run if the typed sentinel is not
        /// `RESET`, unless this flag is set.
        #[arg(long)]
        yes: bool,
        /// Skip the destructive A-D steps and only re-embed any
        /// intakes still in `Extracted`. Use after a `reset` that
        /// aborted mid-run; refuses to run if the library does not
        /// look like an interrupted reset.
        #[arg(long)]
        resume: bool,
    },
}

/// Corpus-side write commands.
#[derive(clap::Subcommand, Debug)]
pub enum CorpusAction {
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

/// Index-stamp reconciliation.
#[derive(clap::Subcommand, Debug)]
pub enum StampsAction {
    /// Probe the embedder for its vector dimension and write the
    /// resulting stamps into `corpus.db`'s `index_meta` when the table
    /// is unstamped. Fails on a stamp mismatch — the operator can then
    /// decide whether to rebuild.
    Reconcile,
}

#[derive(clap::Subcommand, Debug)]
pub enum PapersAction {
    /// Submit a paper file to the glean pipeline. Mirrors the book-side
    /// `ingest` command; with `--recursive`, every supported file under
    /// a directory is enqueued.
    Ingest(PapersIngestArgs),
    /// List papers in catalog order, paginated.
    List(PapersListArgs),
    /// Find papers by title substring, contributor, year, venue, or
    /// DOI.
    Find(PapersFindArgs),
    /// Print the full bibliographic record of one paper by intake id.
    Show {
        /// The intake id of the paper.
        intake_id: i64,
    },
    /// Print the table of contents of one paper.
    Toc {
        /// The intake id of the paper.
        intake_id: i64,
    },
    /// Project one paper's stored bibliographic row onto CSL-JSON and
    /// print it to stdout.
    ExportCsl {
        /// The intake id of the paper.
        intake_id: i64,
    },
    /// Print the locator of one paper's archived source PDF: its
    /// absolute on-disk path, byte size, and SHA-256. The bytes are
    /// not streamed — open the path with the platform's own tools.
    Source {
        /// The intake id of the paper.
        intake_id: i64,
    },
    /// Drop one paper from every paper-side store: the catalog
    /// cascade, the corpus partition, the vector partition, the
    /// envelope file, and the archived source PDF. Audit trail rows
    /// are preserved.
    Remove(PapersRemoveArgs),
    /// Paper-side corpus write commands. Peer of the top-level
    /// `corpus` subcommand for the book pipeline.
    Corpus {
        #[command(subcommand)]
        action: PapersCorpusAction,
    },
    /// Paper-side vector-store write commands. Peer of the top-level
    /// `vectors` subcommand for the book pipeline.
    Vectors {
        #[command(subcommand)]
        action: PapersVectorsAction,
    },
    /// Paper-side index-stamp reconciliation. Peer of the top-level
    /// `stamps` subcommand for the book pipeline.
    Stamps {
        #[command(subcommand)]
        action: PapersStampsAction,
    },
}

/// Paper-side corpus write commands. Peer of [`CorpusAction`].
#[derive(clap::Subcommand, Debug)]
pub enum PapersCorpusAction {
    /// Rebuild `papers_corpus.db` from the v1 extraction envelopes
    /// recorded in `papers_dir`. Intakes whose envelope is missing,
    /// mismatched, or corrupt are reported but skipped.
    Rebuild {
        /// After the corpus tree is rebuilt, also re-embed every
        /// rebuilt paper's abstract chunks. Without this flag the
        /// LanceDB chunks table is left as-is; the index_meta stamps
        /// are reseated from the existing rows.
        #[arg(long)]
        include_vectors: bool,
        /// Restrict the rebuild to one paper intake id. Without this
        /// flag, every paper intake past `Extracted` is rebuilt.
        #[arg(long, value_name = "INTAKE_ID")]
        paper: Option<i64>,
        /// Restrict the rebuild to paper intakes whose stored
        /// `extractor_version` does not equal this binary's
        /// `bookrack_extract::EXTRACTOR_VERSION`. Combines with
        /// `--paper` by intersection.
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

/// Paper-side vector-store write commands. Peer of [`WriteVectorsAction`].
#[derive(clap::Subcommand, Debug)]
pub enum PapersVectorsAction {
    /// Build or rebuild the ANN index over `lancedb_papers`.
    Rebuild {
        /// IVF family — `ivf-flat`, `ivf-sq`, `ivf-pq`, `ivf-hnsw-flat`,
        /// `ivf-hnsw-sq`, `ivf-hnsw-pq`.
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
    /// Drop the ANN index over `lancedb_papers` and mark the meta as
    /// brute-force.
    Drop,
    /// Re-embed every (or a single) paper's chunks in place: read the
    /// existing chunk rows back from `lancedb_papers`, drop their
    /// vectors, and rewrite under the active embedder.
    Reembed {
        /// Restrict the reembed to one paper intake id.
        #[arg(long, value_name = "INTAKE_ID")]
        paper: Option<i64>,
        /// Restrict to paper intakes whose stored `extractor_version`
        /// does not equal this binary's
        /// `bookrack_extract::EXTRACTOR_VERSION`.
        #[arg(long)]
        stale_only: bool,
        /// Print the plan (per-paper chunk counts) and exit without
        /// writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Drop the papers chunks table, clear the papers_corpus index
    /// stamps, and re-derive every paper's abstract chunk with the
    /// env-configured embedding model.
    Reset {
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Skip the destructive A-D steps and only re-embed paper
        /// intakes still in `Extracted`. Use after an aborted reset.
        #[arg(long)]
        resume: bool,
    },
}

/// Paper-side index-stamp reconciliation. Peer of [`StampsAction`].
#[derive(clap::Subcommand, Debug)]
pub enum PapersStampsAction {
    /// Probe the embedder for its vector dimension and write the
    /// resulting stamps into `papers_corpus.db`'s `index_meta`.
    Reconcile,
}

/// Positional + flag bundle for `papers ingest`. Mirrors
/// [`IngestArgs`] for the paper pipeline. `--priority` controls the
/// queue priority of the resulting job.
#[derive(clap::Args, Debug, Clone)]
pub struct PapersIngestArgs {
    pub path: PathBuf,
    #[arg(long)]
    pub recursive: bool,
    #[arg(long)]
    pub force: bool,
    /// Queue priority for the enqueued job: `low`, `normal`, or
    /// `high`. Defaults to `normal`.
    #[arg(long, value_name = "LEVEL")]
    pub priority: Option<String>,
}

/// Pagination bundle for `papers list`.
#[derive(clap::Args, Debug, Clone)]
pub struct PapersListArgs {
    /// Maximum number of papers in this page. Server-side cap applies.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[arg(long)]
    pub offset: Option<u32>,
}

/// Filter bundle for `papers find`. Each flag maps to one filter
/// column on the paper catalog; pass any combination.
#[derive(clap::Args, Debug, Clone)]
pub struct PapersFindArgs {
    /// Substring match against the paper title.
    #[arg(long)]
    pub title: Option<String>,
    /// Exact-equality match against a contributor name.
    #[arg(long)]
    pub contributor: Option<String>,
    /// Exact-equality match against the year column.
    #[arg(long)]
    pub year: Option<String>,
    /// Substring match against the container title (journal,
    /// proceedings, ...).
    #[arg(long)]
    pub venue: Option<String>,
    /// Exact-equality match against the DOI.
    #[arg(long)]
    pub doi: Option<String>,
    /// Maximum number of papers in this page.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[arg(long)]
    pub offset: Option<u32>,
}

/// Positional + flag bundle for `papers remove`. Mirrors
/// [`RemoveArgs`] for the paper pipeline. Either a positional intake
/// id or `--sha <hex>` is required.
#[derive(clap::Args, Debug, Clone)]
pub struct PapersRemoveArgs {
    /// The intake id of the paper to drop. Omit to pass `--sha`.
    pub intake_id: Option<i64>,
    /// Alternative locator: the paper's source SHA-256.
    #[arg(long)]
    pub sha: Option<String>,
    /// Print the plan and exit without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the destructive-action confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(tokens: &[&str]) -> ReplCommand {
        ReplCli::try_parse_from(tokens).expect("parse").command
    }

    #[test]
    fn papers_ingest_carries_path_and_flags() {
        let cmd = parse(&[
            "papers",
            "ingest",
            "/tmp/p.pdf",
            "--force",
            "--priority",
            "high",
        ]);
        match cmd {
            ReplCommand::Papers {
                action: PapersAction::Ingest(args),
            } => {
                assert_eq!(args.path.to_string_lossy(), "/tmp/p.pdf");
                assert!(args.force);
                assert_eq!(args.priority.as_deref(), Some("high"));
                assert!(!args.recursive);
            }
            other => panic!("expected papers ingest, got {other:?}"),
        }
    }

    #[test]
    fn papers_find_collects_filters() {
        let cmd = parse(&[
            "papers",
            "find",
            "--title",
            "attention",
            "--year",
            "2017",
            "--venue",
            "NeurIPS",
        ]);
        match cmd {
            ReplCommand::Papers {
                action: PapersAction::Find(args),
            } => {
                assert_eq!(args.title.as_deref(), Some("attention"));
                assert_eq!(args.year.as_deref(), Some("2017"));
                assert_eq!(args.venue.as_deref(), Some("NeurIPS"));
                assert!(args.doi.is_none());
            }
            other => panic!("expected papers find, got {other:?}"),
        }
    }

    #[test]
    fn papers_export_csl_takes_intake_id() {
        let cmd = parse(&["papers", "export-csl", "42"]);
        match cmd {
            ReplCommand::Papers {
                action: PapersAction::ExportCsl { intake_id },
            } => assert_eq!(intake_id, 42),
            other => panic!("expected papers export-csl, got {other:?}"),
        }
    }

    #[test]
    fn papers_corpus_rebuild_round_trips_flags() {
        let cmd = parse(&[
            "papers",
            "corpus",
            "rebuild",
            "--include-vectors",
            "--paper",
            "7",
            "--stale-only",
            "--dry-run",
            "--yes",
        ]);
        match cmd {
            ReplCommand::Papers {
                action:
                    PapersAction::Corpus {
                        action:
                            PapersCorpusAction::Rebuild {
                                include_vectors,
                                paper,
                                stale_only,
                                dry_run,
                                yes,
                            },
                    },
            } => {
                assert!(include_vectors);
                assert_eq!(paper, Some(7));
                assert!(stale_only);
                assert!(dry_run);
                assert!(yes);
            }
            other => panic!("expected papers corpus rebuild, got {other:?}"),
        }
    }

    #[test]
    fn papers_vectors_reembed_round_trips_flags() {
        let cmd = parse(&[
            "papers",
            "vectors",
            "reembed",
            "--paper",
            "9",
            "--stale-only",
            "--dry-run",
            "--yes",
        ]);
        match cmd {
            ReplCommand::Papers {
                action:
                    PapersAction::Vectors {
                        action:
                            PapersVectorsAction::Reembed {
                                paper,
                                stale_only,
                                dry_run,
                                yes,
                            },
                    },
            } => {
                assert_eq!(paper, Some(9));
                assert!(stale_only);
                assert!(dry_run);
                assert!(yes);
            }
            other => panic!("expected papers vectors reembed, got {other:?}"),
        }
    }

    #[test]
    fn papers_vectors_reset_carries_resume_flag() {
        let cmd = parse(&["papers", "vectors", "reset", "--yes", "--resume"]);
        match cmd {
            ReplCommand::Papers {
                action:
                    PapersAction::Vectors {
                        action: PapersVectorsAction::Reset { yes, resume },
                    },
            } => {
                assert!(yes);
                assert!(resume);
            }
            other => panic!("expected papers vectors reset, got {other:?}"),
        }
    }

    #[test]
    fn papers_stamps_reconcile_takes_no_args() {
        let cmd = parse(&["papers", "stamps", "reconcile"]);
        match cmd {
            ReplCommand::Papers {
                action:
                    PapersAction::Stamps {
                        action: PapersStampsAction::Reconcile,
                    },
            } => {}
            other => panic!("expected papers stamps reconcile, got {other:?}"),
        }
    }
}
