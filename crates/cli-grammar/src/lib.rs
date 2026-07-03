//! Shared `clap::Subcommand` definitions consumed by the top-level
//! `bookrack` CLI. Each subcommand and its argument struct lives here
//! so the binary crate and the daemon-side runner reach the same
//! grammar without a runtime dependency on the daemon stack.

use std::path::PathBuf;

/// Lifecycle actions on the persistent ingest queue. Dispatch maps
/// each variant to the matching control-plane RPC.
#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    /// List every row in the queue document, oldest first.
    List {
        /// Print full UUIDv7 job ids instead of the 8-character
        /// prefix the table shows by default.
        #[arg(long)]
        long: bool,
    },
    /// Pause the worker loop. Running jobs run to completion; pending
    /// rows stay pending until `resume`.
    Pause,
    /// Resume the worker loop, allowing it to pull pending rows again.
    Resume,
    /// Cancel every pending row in one sweep. Running jobs are left
    /// alone.
    Clear,
    /// Cancel the unique job whose id starts with `<JOB_ID>`.
    ///
    /// Empty prefixes are rejected. An ambiguous prefix returns an
    /// error without cancelling anything.
    Cancel {
        /// Prefix of the job's UUIDv7 to cancel. The first eight
        /// characters listed by `queue list` are usually enough.
        job_id: String,
    },
}

/// Positional + flag bundle for `ingest`. Lives in a standalone struct
/// so the same type can be embedded in the top-level `Command::Ingest`
/// variant without duplicating attributes.
#[derive(clap::Args, Debug, Clone)]
pub struct IngestArgs {
    /// Source file, or a directory the ingest walks recursively (with
    /// `--recursive`).
    pub path: PathBuf,
    /// Walk `<PATH>` as a directory and enqueue every supported file.
    /// Without this flag, `<PATH>` must point at a single book file.
    #[arg(long)]
    pub recursive: bool,
    /// Pause each book after EXTRACT so a curator can review the audit
    /// verdict before CHUNK / EMBED. Resume with `metadata advance`,
    /// `metadata approve`, or any of the other gate decisions.
    #[arg(long)]
    pub hold_for_metadata: bool,
    /// Re-ingest even when the source SHA-256 already exists in the
    /// catalog. Without this flag, duplicate sources are skipped.
    #[arg(long)]
    pub force: bool,
    /// Queue priority for the enqueued job(s): `low`, `normal`, or
    /// `high`. Defaults to `normal`.
    #[arg(long, value_name = "LEVEL")]
    pub priority: Option<String>,
    /// Skip waiting for the enqueued job(s) to finish. Without this
    /// flag, the command stays attached until every job reaches a
    /// terminal state (`Done` / `Failed` / `Cancelled`) and then
    /// prints a one-line human summary; the historical behaviour
    /// returned as soon as the daemon had the job in its queue.
    #[arg(long = "no-wait")]
    pub no_wait: bool,
}

/// Positional + flag bundle for `remove`.
#[derive(clap::Args, Debug, Clone)]
#[command(group(
    clap::ArgGroup::new("remove_target")
        .required(true)
        .multiple(false)
        .args(["intake_id", "sha"])
))]
pub struct RemoveArgs {
    /// Intake id of the book to drop. Mutually exclusive with `--sha`;
    /// exactly one of the two must be supplied.
    pub intake_id: Option<i64>,
    /// Drop the book whose source SHA-256 starts with this hex prefix.
    /// Mutually exclusive with the positional intake id.
    #[arg(long, value_name = "HEX")]
    pub sha: Option<String>,
    /// Print the per-store removal plan and exit without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the destructive-action confirmation prompt.
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

/// Intake-side commands: the OCR product re-entry point and the
/// worklist of scan sources still awaiting OCR.
#[derive(clap::Subcommand, Debug)]
pub enum IntakeAction {
    /// List scan sources still awaiting OCR.
    ///
    /// Each row is a source the extract stage rejected as image-only:
    /// registered as a `needs_ocr` anchor with no OCR product yet. The
    /// output names the path to hand to an external OCR tool, the
    /// source hash, a best-effort page count, and why the text layer
    /// was rejected. Run OCR with any tool, then re-enter the product
    /// through `intake ocr`. `--json` emits the full manifest.
    ListOcrPending {
        /// Maximum number of sources to list. Omitted or zero uses the
        /// default page size; the value is clamped to the read cap.
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
        /// Number of sources to skip before the page starts.
        #[arg(long, value_name = "N")]
        offset: Option<u32>,
    },
    /// Bring an OCR product into the library as a derived source.
    ///
    /// The scan PDF named by `--from-pdf` is registered as the durable
    /// source anchor (status `needs_ocr`); the OCR markdown is
    /// registered as its own intake whose `Provenance` forensically
    /// references the PDF's hash and flows through STRUCTURE / CHUNK /
    /// EMBED. The expected page count comes from the source PDF's
    /// `/Pages`; pass `--expected-pages` to override it when PDFium
    /// cannot read the source, and `--allow-partial` to accept an OCR
    /// product whose sheets do not cover every page.
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
        /// Re-ingest even when either the OCR markdown or the scan PDF
        /// already has an entry under its SHA-256. Without this flag,
        /// duplicate sources are skipped.
        #[arg(long)]
        force: bool,
        /// Park the resulting book at STRUCTURE if the audit verdict
        /// is `needs_work`, skipping CHUNK and EMBED until a curator
        /// drives it past the metadata gate. Mirrors the `ingest`
        /// flag of the same name.
        #[arg(long)]
        hold_for_metadata: bool,
        /// Queue priority for the enqueued job: `low`, `normal`, or
        /// `high`. Defaults to `normal`.
        #[arg(long, value_name = "LEVEL")]
        priority: Option<String>,
        /// Skip waiting for the enqueued job to finish. Without this
        /// flag, the command stays attached until the OCR intake
        /// reaches a terminal state and prints a one-line human
        /// summary; the historical behaviour returned as soon as
        /// the daemon had the job in its queue.
        #[arg(long = "no-wait")]
        no_wait: bool,
    },
}

/// Distill-side commands. The CLI runs these in-process against
/// `<data>/reference.db`; no daemon RPC is involved. The positional
/// `<PATH>` arguments mirror the ingest / glean / intake surface: a
/// path can be a `book.toml` file, a directory holding one, or a
/// source file with a co-located `book.toml`.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum DistillAction {
    /// Build distilled entries for the books reachable from `<PATHS>`.
    Build(DistillBuildArgs),
    /// Re-run distill in memory and diff against the live database.
    Verify(DistillVerifyArgs),
    /// Statically validate each `book.toml` against the catalogs and
    /// run a per-stage retention report against a truncated source
    /// sample, without touching the database.
    Lint(DistillLintArgs),
    /// List the registered reference books and their entry counts.
    List(DistillListArgs),
}

/// Positional + flag bundle for `distill build`.
#[derive(clap::Args, Debug, Clone)]
pub struct DistillBuildArgs {
    /// One or more `book.toml` files, source files, or directories.
    /// A directory is expanded under `--recursive`; multiple paths run
    /// independently. Mirrors the `bookrack ingest <PATH>...` shape.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// Walk directories recursively when discovering `book.toml`.
    #[arg(long)]
    pub recursive: bool,
    /// Run the pipeline but do not write to `reference.db`; print the
    /// coverage summary instead.
    #[arg(long)]
    pub dry_run: bool,
    /// Lower bound, expressed as a fraction in `[0.0, 1.0]`, for any
    /// same-kind stage's output-to-input ratio. A stage that falls
    /// below this threshold is treated as a probable misconfiguration:
    /// the run prints the dropped-sample lines and exits non-zero.
    /// Cross-kind stages (source -> pages, etc.) are not bounded.
    #[arg(long, value_name = "FRACTION", default_value_t = 0.10)]
    pub retention_threshold: f64,
    /// Disable the per-stage retention guard; the per-stage table is
    /// still printed for inspection, but a low-retention row no
    /// longer fails the run. Operators can use this to inspect a
    /// known-aggressive pipeline without flag noise.
    #[arg(long)]
    pub no_retention_check: bool,
    /// Skip writing the run's audit row into `catalog.db`. The
    /// pipeline still runs and (unless `--dry-run`) the drafts still
    /// reach `reference.db`; this flag exists so CI checks and
    /// reproducible parameter sweeps do not pollute the audit table.
    #[arg(long)]
    pub no_audit_write: bool,
}

/// Positional + flag bundle for `distill verify`.
#[derive(clap::Args, Debug, Clone)]
pub struct DistillVerifyArgs {
    /// One or more `book.toml` files, source files, or directories.
    /// Resolved the same way as `distill build`.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// Walk directories recursively when discovering `book.toml`.
    #[arg(long)]
    pub recursive: bool,
}

/// Positional + flag bundle for `distill lint`. The command keeps
/// going across books so a multi-book run surfaces every problem at
/// once instead of failing on the first; the process exits non-zero
/// when any book failed.
#[derive(clap::Args, Debug, Clone)]
pub struct DistillLintArgs {
    /// One or more `book.toml` files, source files, or directories.
    /// Resolved the same way as `distill build`.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// Walk directories recursively when discovering `book.toml`.
    #[arg(long)]
    pub recursive: bool,
    /// Cap on the number of source lines fed to the sample run. The
    /// resolved source is truncated to this many lines so the
    /// retention table prints in seconds rather than grinding through
    /// a multi-megabyte OCR product. Zero disables the sample run and
    /// limits the lint to the static catalog checks.
    #[arg(long, value_name = "N", default_value_t = 300)]
    pub sample_lines: usize,
}

/// Flag bundle for `distill list`. Currently empty; rendering mode is
/// controlled by the global `--json` flag through `render::ctx`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct DistillListArgs {}

/// `bookrack runs` — list and inspect pipeline runs. A run row is
/// opened by every top-level command that drives a pipeline; its
/// rollup is refreshed at run close when the command writes any audit
/// row.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum RunsAction {
    /// List recent `pipeline_runs` rows.
    List {
        /// Cap the result to the most recent N runs. Default is no cap.
        #[arg(long, value_name = "N")]
        last: Option<usize>,
        /// Filter to one command name (e.g. `distill_build`, `ingest`,
        /// `dryrun`).
        #[arg(long, value_name = "NAME")]
        command: Option<String>,
    },
    /// Show one run by id, with its summary verdict / flag / coverage
    /// distributions rendered as horizontal histograms.
    Show {
        /// The `pipeline_runs.pipeline_run_id` to show. The composite
        /// form is `<command>-<ISO8601>-<sha8>`.
        run_id: String,
    },
}

/// `bookrack retrieval` — list and inspect recorded retrieval calls.
/// One sidecar row lands per single-store `search` invocation, keyed
/// by the corpus fingerprint that served it, with the returned hits
/// in rank order.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum RetrievalAction {
    /// List recent retrieval calls.
    List {
        /// Cap the result to the most recent N calls. Default is no cap.
        #[arg(long, value_name = "N")]
        last: Option<usize>,
        /// Filter to calls served by one corpus fingerprint (16 hex
        /// characters).
        #[arg(long, value_name = "HEX")]
        corpus_fingerprint: Option<String>,
    },
    /// Show one retrieval call by id: call metadata and its hits in
    /// rank order.
    Show {
        /// The `mcp_tool_calls.call_id` the retrieval was logged under.
        call_id: i64,
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
    /// Suppress a field's extracted value without supplying a replacement.
    ///
    /// The field reads as absent until a correct value is set. `clear`
    /// removes the suppression.
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
    /// Re-run the metadata audit against the book's cached extraction.
    ///
    /// Refreshes the stored verdict and confidence so they reflect the
    /// current effective metadata. The review status is untouched.
    Reaudit {
        /// The intake id of the book.
        book: i64,
    },
    /// Attribute a contributor to the book.
    ///
    /// Adds a row with origin `user`, appended after the role's existing
    /// contributors. User rows survive a re-ingest.
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
    /// Remove one contributor row by its surrogate id.
    ///
    /// The id is the one listed by `show_book`. The row is removed
    /// whatever its origin — this is the path for stripping a wrong
    /// extracted attribution.
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
    /// Acknowledge a metadata gap and let the book through.
    ///
    /// Signs the override with a reason for the audit trail.
    Ack {
        /// The intake id of the book.
        book: i64,
        /// Why the gap was accepted.
        #[arg(long)]
        reason: String,
    },
    /// Mark the record reviewed and correct.
    ///
    /// A human or LLM uses this after confirming the metadata; the
    /// pipeline never writes this status itself.
    Approve {
        /// The intake id of the book.
        book: i64,
        /// Optional note for the audit trail.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Reject the book outright.
    ///
    /// Suitable for e.g. a wrong source file or irrecoverable metadata.
    /// The book stays ingested but downstream consumers can filter on
    /// the rejected status.
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
    /// Build or rebuild the ANN index.
    ///
    /// Without any flag, reads the persisted config from
    /// `vectors_meta.json` and rebuilds from that — useful after corpus
    /// growth has exceeded the L2 churn threshold. Any explicit flag
    /// overrides the meta for that parameter.
    Rebuild {
        /// IVF family — `ivf-flat`, `ivf-sq`, `ivf-pq`, `ivf-hnsw-flat`,
        /// `ivf-hnsw-sq`, `ivf-hnsw-pq`. Defaults to whatever the meta
        /// holds, or `ivf-flat` for a fresh library.
        #[arg(long)]
        kind: Option<String>,
        /// IVF coarse-cluster count. Higher values shrink each list but
        /// raise rebuild cost. Defaults to whatever the meta holds, or
        /// roughly `sqrt(n_rows)` for a fresh library.
        #[arg(long)]
        num_partitions: Option<u32>,
        /// PQ sub-quantizer count (only for `ivf-pq` and `ivf-hnsw-pq`).
        /// Must divide the embedding dimension.
        #[arg(long)]
        num_sub_vectors: Option<u32>,
        /// PQ codebook bit width per sub-vector (only for the PQ
        /// families).
        #[arg(long)]
        num_bits: Option<u32>,
        /// Default number of partitions to scan at query time. Trades
        /// recall for latency.
        #[arg(long)]
        nprobes: Option<u32>,
        /// Rescore the top `refine_factor * k` ANN hits with exact
        /// distance before returning `k`. Higher values trade latency
        /// for recall.
        #[arg(long)]
        refine_factor: Option<u32>,
    },
    /// Drop the ANN index and mark the meta as brute-force.
    ///
    /// Search falls back to a full scan on the next query. The index
    /// is not recoverable without a `vectors rebuild`; the daemon
    /// rejects the call without `--yes`.
    Drop {
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Re-embed every (or a single) book's chunks in place.
    ///
    /// Reads the existing chunk rows back from LanceDB, drops their
    /// vectors, runs them back through the active embedder, and writes
    /// them as the new vectors. Use when the chunking or normalization
    /// algorithm bumped; for an embedding model swap use `libraries
    /// fork` or `vectors reset`.
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
    /// Drop and rebuild every vector under the active embedding model.
    ///
    /// Drops the chunks table, clears the corpus index stamps, and
    /// re-derives every book's vectors with the env-configured embedding
    /// model. Use after switching `BOOKRACK_EMBED_MODEL`. The old
    /// vectors are unrecoverable; consider `libraries fork` for a
    /// non-destructive trial first.
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
    /// Rebuild `corpus.db` from the v1 extraction envelopes.
    ///
    /// Reads each intake's envelope from the opaque store and writes the
    /// corpus tree afresh. Intakes whose envelope is missing,
    /// mismatched, or corrupt are reported but skipped.
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
    /// Probe the embedder and stamp `corpus.db`'s `index_meta`.
    ///
    /// Probes the embedder for its vector dimension and writes the
    /// resulting stamps into `corpus.db`'s `index_meta` when the table
    /// is unstamped. Fails on a stamp mismatch — the operator can then
    /// decide whether to rebuild.
    Reconcile,
}

#[derive(clap::Subcommand, Debug)]
pub enum PapersAction {
    /// Submit a paper file to the glean pipeline.
    ///
    /// Mirrors the book-side `ingest` command. With `--recursive`,
    /// every supported file under a directory is enqueued.
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
    /// Print the locator of one paper's archived source PDF.
    ///
    /// Reports the absolute on-disk path, byte size, and SHA-256. The
    /// bytes are not streamed — open the path with the platform's own
    /// tools.
    Source {
        /// The intake id of the paper.
        intake_id: i64,
    },
    /// Drop one paper from every paper-side store.
    ///
    /// Removes the catalog cascade, the corpus partition, the vector
    /// partition, the envelope file, and the archived source PDF.
    /// Audit trail rows are preserved.
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
    /// Simulate a paper ingest without writing the live stores.
    ///
    /// Runs each step up to (but not including) embedding and writes a
    /// JSONL report of IDENTIFY hit rates and predicted STRUCTURE
    /// stats. The real catalog, corpus, and vector store are not
    /// touched.
    Dryrun(PapersDryrunArgs),
    /// Paper-side metadata curation commands. Peer of the top-level
    /// `metadata` subcommand for the book pipeline.
    Metadata {
        #[command(subcommand)]
        action: PapersMetadataAction,
    },
}

/// Paper-side metadata curation actions. Peer of `MetadataAction`
/// for the books pipeline; currently exposes the `reaudit` action.
#[derive(clap::Subcommand, Debug)]
pub enum PapersMetadataAction {
    /// Re-run the paper-side metadata audit on an existing intake's
    /// cached extraction. Writes only the `confidence` /
    /// `audit_verdict` rollup; the base attrs, contributors, and
    /// review status all stay as they are.
    Reaudit {
        /// The intake id of the paper to re-audit.
        intake_id: i64,
        /// Optional named audit profile. When absent the daemon's
        /// effective profile (default + overlay) is used.
        #[arg(long)]
        audit_profile: Option<String>,
    },
    /// Override one field on a paper's effective record.
    Set {
        /// Intake id of the paper.
        intake_id: i64,
        /// Column on the paper attrs row to override (e.g. `title`,
        /// `year`, `container_title`, `doi`).
        #[arg(long)]
        field: String,
        /// The new value.
        #[arg(long)]
        value: String,
        /// Mark the override as confirmed against the source.
        #[arg(long)]
        confirmed: bool,
    },
    /// Remove an override on one field, reverting to the extracted
    /// value.
    Clear {
        /// Intake id of the paper.
        intake_id: i64,
        /// The field whose override is removed.
        #[arg(long)]
        field: String,
    },
    /// Set an override that deliberately voids one field's value.
    Void {
        /// Intake id of the paper.
        intake_id: i64,
        /// The field whose extracted value is suppressed.
        #[arg(long)]
        field: String,
    },
    /// Acknowledge a flagged paper without changing its metadata —
    /// move the review row to `acknowledged`.
    Ack {
        /// Intake id of the paper.
        intake_id: i64,
        /// Optional note for the audit trail.
        #[arg(long)]
        notes: Option<String>,
    },
    /// Approve a paper's metadata as correct.
    Approve {
        /// Intake id of the paper.
        intake_id: i64,
        /// Optional note for the audit trail.
        #[arg(long)]
        notes: Option<String>,
    },
    /// Reject a paper's metadata as wrong.
    Reject {
        /// Intake id of the paper.
        intake_id: i64,
        /// Optional note for the audit trail.
        #[arg(long)]
        notes: Option<String>,
    },
    /// Move a previously approved / rejected paper back to
    /// `pending`.
    Reopen {
        /// Intake id of the paper.
        intake_id: i64,
        /// Optional note for the audit trail.
        #[arg(long)]
        notes: Option<String>,
    },
    /// Add a contributor row to a paper.
    ContributorAdd {
        /// Intake id of the paper.
        intake_id: i64,
        /// Contribution role (author / editor / translator / other).
        #[arg(long)]
        role: String,
        /// The contributor's display name, used when family / given
        /// cannot be separated.
        #[arg(long)]
        name: String,
        /// The contributor's family name, when separable.
        #[arg(long)]
        family: Option<String>,
        /// The contributor's given name, when separable.
        #[arg(long)]
        given: Option<String>,
        /// The contributor's ORCID identifier, when known.
        #[arg(long)]
        orcid: Option<String>,
    },
    /// Remove a contributor row by id.
    ContributorRemove {
        /// Surrogate id of the contributor row to remove (listed by
        /// `papers show`).
        contributor_id: i64,
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
        /// IVF coarse-cluster count. Higher values shrink each list but
        /// raise rebuild cost. Defaults to whatever the meta holds, or
        /// roughly `sqrt(n_rows)` for a fresh library.
        #[arg(long)]
        num_partitions: Option<u32>,
        /// PQ sub-quantizer count (only for `ivf-pq` and `ivf-hnsw-pq`).
        /// Must divide the embedding dimension.
        #[arg(long)]
        num_sub_vectors: Option<u32>,
        /// PQ codebook bit width per sub-vector (only for the PQ
        /// families).
        #[arg(long)]
        num_bits: Option<u32>,
        /// Default number of partitions to scan at query time. Trades
        /// recall for latency.
        #[arg(long)]
        nprobes: Option<u32>,
        /// Rescore the top `refine_factor * k` ANN hits with exact
        /// distance before returning `k`. Higher values trade latency
        /// for recall.
        #[arg(long)]
        refine_factor: Option<u32>,
    },
    /// Drop the ANN index over `lancedb_papers` and mark the meta as
    /// brute-force. Peer of [`WriteVectorsAction::Drop`]; the daemon
    /// rejects the call without `--yes`.
    Drop {
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
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

/// Positional + flag bundle for `papers dryrun`.
#[derive(clap::Args, Debug, Clone)]
pub struct PapersDryrunArgs {
    /// Source PDF / EPUB / TXT / HTML, or a directory the dryrun
    /// walks recursively.
    pub path: PathBuf,
    /// Write the per-paper report to this path instead of the default
    /// `<data_root>/dryruns/dryrun-paper-...` location. Implies the
    /// summary is written alongside with a `.summary.json` suffix.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Write JSONL to stdout instead of to a file. The summary still
    /// lands on stderr at the end of the run.
    #[arg(long)]
    pub stdout: bool,
    /// Skip the CHUNK preview. Saves a few milliseconds per file when
    /// only the IDENTIFY hit rates are wanted.
    #[arg(long)]
    pub no_chunk: bool,
}

/// Positional + flag bundle for `papers ingest`. Mirrors
/// [`IngestArgs`] for the paper pipeline. `--priority` controls the
/// queue priority of the resulting job.
#[derive(clap::Args, Debug, Clone)]
pub struct PapersIngestArgs {
    /// Source file, or a directory the ingest walks recursively (with
    /// `--recursive`).
    pub path: PathBuf,
    /// Walk `<PATH>` as a directory and enqueue every supported file.
    /// Without this flag, `<PATH>` must point at a single paper file.
    #[arg(long)]
    pub recursive: bool,
    /// Re-ingest even when the source SHA-256 already exists in the
    /// paper catalog. Without this flag, duplicate sources are skipped.
    #[arg(long)]
    pub force: bool,
    /// Queue priority for the enqueued job: `low`, `normal`, or
    /// `high`. Defaults to `normal`.
    #[arg(long, value_name = "LEVEL")]
    pub priority: Option<String>,
    /// Skip waiting for the enqueued job(s) to finish. Without this
    /// flag, the command stays attached until every job reaches a
    /// terminal state (`Done` / `Failed` / `Cancelled`) and then
    /// prints a one-line human summary; the historical behaviour
    /// returned as soon as the daemon had the job in its queue.
    #[arg(long = "no-wait")]
    pub no_wait: bool,
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

/// Flag bundle for the top-level `bookrack logs` command. `--tail N`
/// snapshots the most recent N events from the daemon's in-memory
/// ring (capped server-side at 1024); `--follow` streams new events
/// from the broadcast as they arrive; with neither flag the command
/// defaults to follow. `--level LEVEL` filters to events at or above
/// the given severity.
#[derive(clap::Args, Debug, Clone)]
pub struct LogsArgs {
    /// Snapshot the most recent N events from the daemon's ring and
    /// print them. When combined with `--follow`, the snapshot is
    /// emitted before the live stream begins.
    #[arg(long, value_name = "N")]
    pub tail: Option<u64>,
    /// Stream events as they arrive from the daemon's broadcast.
    /// Defaults to true when neither this flag nor `--tail` is set.
    #[arg(long)]
    pub follow: bool,
    /// Drop every event below this severity. Case-insensitive;
    /// accepted values are `TRACE`, `DEBUG`, `INFO`, `WARN`, and
    /// `ERROR`. Without this flag every level passes through.
    #[arg(long, value_name = "LEVEL")]
    pub level: Option<String>,
}

/// Positional + flag bundle for `papers remove`. Mirrors
/// [`RemoveArgs`] for the paper pipeline. Either a positional intake
/// id or `--sha <hex>` is required.
#[derive(clap::Args, Debug, Clone)]
#[command(group(
    clap::ArgGroup::new("papers_remove_target")
        .required(true)
        .multiple(false)
        .args(["intake_id", "sha"])
))]
pub struct PapersRemoveArgs {
    /// The intake id of the paper to drop. Mutually exclusive with
    /// `--sha`; exactly one of the two must be supplied.
    pub intake_id: Option<i64>,
    /// Drop the paper whose source SHA-256 starts with this hex
    /// prefix. Mutually exclusive with the positional intake id.
    #[arg(long, value_name = "HEX")]
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

    /// Test-only wrapper so the leaf `PapersAction` enum can be parsed
    /// from a raw token list without depending on the top-level CLI.
    #[derive(clap::Parser, Debug)]
    #[command(name = "", no_binary_name = true)]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(clap::Subcommand, Debug)]
    enum TestCommand {
        Papers {
            #[command(subcommand)]
            action: PapersAction,
        },
        Ingest(IngestArgs),
        Intake {
            #[command(subcommand)]
            action: IntakeAction,
        },
    }

    fn parse(tokens: &[&str]) -> PapersAction {
        match TestCli::try_parse_from(tokens).expect("parse").command {
            TestCommand::Papers { action } => action,
            other => panic!("expected papers, got {other:?}"),
        }
    }

    fn parse_ingest(tokens: &[&str]) -> IngestArgs {
        match TestCli::try_parse_from(tokens).expect("parse").command {
            TestCommand::Ingest(args) => args,
            other => panic!("expected ingest, got {other:?}"),
        }
    }

    fn parse_intake(tokens: &[&str]) -> IntakeAction {
        match TestCli::try_parse_from(tokens).expect("parse").command {
            TestCommand::Intake { action } => action,
            other => panic!("expected intake, got {other:?}"),
        }
    }

    #[test]
    fn ingest_carries_priority_flag() {
        let args = parse_ingest(&[
            "ingest",
            "/tmp/b.epub",
            "--force",
            "--hold-for-metadata",
            "--priority",
            "high",
        ]);
        assert_eq!(args.path.to_string_lossy(), "/tmp/b.epub");
        assert!(args.force);
        assert!(args.hold_for_metadata);
        assert!(!args.recursive);
        assert!(!args.no_wait);
        assert_eq!(args.priority.as_deref(), Some("high"));
    }

    #[test]
    fn ingest_priority_is_optional() {
        let args = parse_ingest(&["ingest", "/tmp/b.epub"]);
        assert!(args.priority.is_none());
    }

    #[test]
    fn intake_ocr_carries_priority_force_hold_flags() {
        let action = parse_intake(&[
            "intake",
            "ocr",
            "/tmp/out.md",
            "--from-pdf",
            "/tmp/scan.pdf",
            "--force",
            "--hold-for-metadata",
            "--priority",
            "low",
        ]);
        match action {
            IntakeAction::Ocr {
                ocr_md,
                from_pdf,
                force,
                hold_for_metadata,
                priority,
                allow_partial,
                expected_pages,
                no_wait,
            } => {
                assert_eq!(ocr_md.to_string_lossy(), "/tmp/out.md");
                assert_eq!(from_pdf.to_string_lossy(), "/tmp/scan.pdf");
                assert!(force);
                assert!(hold_for_metadata);
                assert_eq!(priority.as_deref(), Some("low"));
                assert!(!allow_partial);
                assert!(expected_pages.is_none());
                assert!(!no_wait);
            }
            other => panic!("expected ocr action, got {other:?}"),
        }
    }

    #[test]
    fn intake_ocr_defaults_when_priority_force_hold_omitted() {
        let action = parse_intake(&[
            "intake",
            "ocr",
            "/tmp/out.md",
            "--from-pdf",
            "/tmp/scan.pdf",
        ]);
        match action {
            IntakeAction::Ocr {
                force,
                hold_for_metadata,
                priority,
                ..
            } => {
                assert!(!force);
                assert!(!hold_for_metadata);
                assert!(priority.is_none());
            }
            other => panic!("expected ocr action, got {other:?}"),
        }
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
            PapersAction::Ingest(args) => {
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
            PapersAction::Find(args) => {
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
            PapersAction::ExportCsl { intake_id } => assert_eq!(intake_id, 42),
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
            PapersAction::Corpus {
                action:
                    PapersCorpusAction::Rebuild {
                        include_vectors,
                        paper,
                        stale_only,
                        dry_run,
                        yes,
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
            PapersAction::Vectors {
                action:
                    PapersVectorsAction::Reembed {
                        paper,
                        stale_only,
                        dry_run,
                        yes,
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
            PapersAction::Vectors {
                action: PapersVectorsAction::Reset { yes, resume },
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
            PapersAction::Stamps {
                action: PapersStampsAction::Reconcile,
            } => {}
            other => panic!("expected papers stamps reconcile, got {other:?}"),
        }
    }

    #[test]
    fn papers_dryrun_carries_path_and_flags() {
        let cmd = parse(&[
            "papers",
            "dryrun",
            "/papers",
            "--out",
            "/tmp/r.jsonl",
            "--no-chunk",
            "--stdout",
        ]);
        match cmd {
            PapersAction::Dryrun(args) => {
                assert_eq!(args.path.to_string_lossy(), "/papers");
                assert_eq!(
                    args.out.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    Some("/tmp/r.jsonl".to_string())
                );
                assert!(args.no_chunk);
                assert!(args.stdout);
            }
            other => panic!("expected papers dryrun, got {other:?}"),
        }
    }

    #[test]
    fn papers_remove_accepts_either_locator() {
        for argv in [
            vec!["papers", "remove", "42"],
            vec!["papers", "remove", "42", "--dry-run"],
            vec!["papers", "remove", "--sha", "deadbeef"],
            vec!["papers", "remove", "--sha", "deadbeef", "--yes"],
        ] {
            TestCli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|_| panic!("argv must parse: {argv:?}"));
        }
    }

    #[test]
    fn papers_remove_requires_a_locator() {
        let Err(err) = TestCli::try_parse_from(["papers", "remove", "--dry-run"]) else {
            panic!("an empty selector must error");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn papers_remove_rejects_both_intake_id_and_sha_together() {
        let Err(err) = TestCli::try_parse_from(["papers", "remove", "42", "--sha", "deadbeef"])
        else {
            panic!("the two selectors must not be combined");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
