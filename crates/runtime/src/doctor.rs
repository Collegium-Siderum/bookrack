// SPDX-License-Identifier: Apache-2.0

//! `bookrack doctor`: one-screen health check of an install.
//!
//! Each environment expectation — a resolved data root, the on-disk
//! presence of each database store, a loadable PDFium library, a
//! sufficient file-descriptor limit, a reachable Ollama daemon
//! carrying the configured embed model — becomes one row in a fixed
//! three-column table. A row is `OK`,
//! `WARN`, or `FAIL`; any FAIL exits the process with status 1 so a
//! script can branch on the result.
//!
//! The store-presence rows deliberately stop at `path.exists()`: a
//! read-write open would apply pending migrations and contend for the
//! daemon's exclusive write lock. The registry-coherence rows do open
//! the corpus read-only to read its built stamps — a `query_only` WAL
//! open takes no write lock and is safe alongside a running daemon —
//! but no row opens a store read-write. Deeper introspection lives
//! behind the REPL `status` command instead.
//!
//! The command runs **before** `Config::resolve`, so an unconfigured
//! install still produces a row stating that — rather than the resolver
//! short-circuiting the very diagnosis the user needs.

use std::path::Path;

use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, ConfigError, DEFAULT_EMBED_MODEL, DEFAULT_OLLAMA_URL, LibraryEntry,
    LibraryIdentification, LibrarySelection, ManifestError, ProfileRefDrift, ProfileRefOrigin,
    ResolutionSource, ShadowedDefault, default_registry_path, effective_profile_reference,
    list_libraries, load_manifest, locate_pdfium, pdfium_library_filename, profile_reference_drift,
};
use bookrack_embed::{DEFAULT_PROBE_TIMEOUT, pull_command};
use bookrack_index_profile::{has_errors, resolve, validate};
use eyre::{Context, Result};
use serde::Serialize;

use crate::backend_probe::{EmbedBackendState, check_embed_backend};
use crate::mcp_endpoint::McpEndpointState;

/// One row of the health report.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct Row {
    /// Short label rendered in the first column.
    pub label: String,
    /// Observed value rendered in the second column.
    pub value: String,
    /// Status and optional explanatory note.
    #[serde(flatten)]
    pub status: Status,
}

/// Outcome of one check. `note` carries the actionable hint for the
/// non-OK paths so a user can pipe `bookrack doctor` to a bug report
/// without rerunning anything.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Status {
    Ok {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        note: Option<String>,
    },
    Warn {
        note: String,
    },
    Fail {
        note: String,
    },
}

impl Status {
    fn is_fail(&self) -> bool {
        matches!(self, Status::Fail { .. })
    }
}

/// Materialised report with a deterministic row order. Tests build one
/// of these directly against tempdirs so the renderer can stay pure.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct Report {
    pub rows: Vec<Row>,
}

impl Report {
    /// `true` iff at least one row failed.
    pub fn has_failures(&self) -> bool {
        self.rows.iter().any(|r| r.status.is_fail())
    }

    /// Number of failed rows.
    pub fn failure_count(&self) -> usize {
        self.rows.iter().filter(|r| r.status.is_fail()).count()
    }
}

/// CLI entry point. Resolves config without erroring on a missing one,
/// gathers every check, and renders the report. Returns `true` when
/// every check passes and `false` when at least one row is FAIL.
/// The boolean is returned, not bailed, so the call site can map an
/// expected "not ready" health outcome to a non-zero exit code
/// without adding an extra error line on top of the table the
/// renderer already wrote.
pub async fn run(selection: &LibrarySelection, json: bool) -> Result<bool> {
    let report = gather(selection).await;
    if json {
        render_json(&report);
    } else {
        render_text(&report);
    }
    Ok(!report.has_failures())
}

/// Render a [`Report`] previously returned by the control-plane
/// `doctor.gather` RPC. The CLI-side `bookrack doctor` client calls
/// this to keep the text/JSON output identical between the
/// daemon-running and daemon-not-running paths.
pub fn render_value(value: &serde_json::Value, json: bool) -> Result<bool> {
    let report: Report =
        serde_json::from_value(value.clone()).context("decode doctor.gather response")?;
    if json {
        render_json(&report);
    } else {
        render_text(&report);
    }
    Ok(!report.has_failures())
}

/// Build a [`Report`] for the given selection. Pure over its inputs in
/// the sense that every observation is fresh — there is no in-process
/// cache to invalidate between successive calls.
pub async fn gather(selection: &LibrarySelection) -> Report {
    gather_with(selection, None, None).await
}

/// [`gather`] with what only a live daemon knows: its supervised
/// reranker backend, so that row reports the supervisor state instead
/// of `not running`, and the MCP address it actually bound, so the
/// endpoint row asks about the address being served rather than the
/// one configured. The daemon's `doctor.gather` handler passes both;
/// the offline CLI path passes neither.
pub async fn gather_with(
    selection: &LibrarySelection,
    rerank_supervisor: Option<&crate::rerank_supervisor::RerankSupervisor>,
    served_mcp_addr: Option<&str>,
) -> Report {
    let mut rows = Vec::new();

    let cfg = push_data_root_row(&mut rows, selection);
    push_pdfium_row(&mut rows);
    push_fd_limit_row(&mut rows);
    if let Some(cfg) = &cfg {
        push_catalog_row(&mut rows, cfg);
        push_corpus_row(&mut rows, cfg);
    }
    // One resolution feeds both registry-backed sections, so they can
    // never disagree about what the registry says.
    let registry = probe_registry(list_libraries());
    push_registry_consistency_rows(&mut rows, &registry);
    push_index_profile_coherence_rows(&mut rows, &registry);
    let ollama_url = ollama_url_for_probe(cfg.as_ref());
    let embed_model = embed_model_for_probe(cfg.as_ref());
    push_ollama_rows(&mut rows, &ollama_url, &embed_model).await;
    push_reranker_rows(&mut rows, cfg.as_ref(), rerank_supervisor).await;
    push_mcp_endpoint_row(&mut rows, served_mcp_addr).await;

    Report { rows }
}

/// Outcome of one envelope-rename run, surfaced verbatim through the
/// CLI text and JSON renderers so an operator can audit what moved.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RenameReport {
    /// `true` when no rename was actually performed; the `renamed`
    /// list then carries the plan that a real run would have applied.
    pub dry_run: bool,
    /// Per-file plan or applied move, in scan order.
    pub renamed: Vec<RenameAction>,
    /// Number of files skipped because their basename already carried
    /// a `book-` or `paper-` prefix.
    pub already_prefixed: usize,
    /// Per-file failures, in scan order. A failure on one file does
    /// not stop the rest of the batch.
    pub failures: Vec<RenameFailure>,
}

impl RenameReport {
    /// `true` iff any file failed to rename.
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// One envelope to move from its legacy basename to its kinded one.
#[derive(Debug, Clone, Serialize)]
pub struct RenameAction {
    pub kind: String,
    pub from: String,
    pub to: String,
}

/// One envelope that could not be moved.
#[derive(Debug, Clone, Serialize)]
pub struct RenameFailure {
    pub path: String,
    pub error: String,
}

/// Walk the books and papers opaque stores, migrate legacy-named
/// envelopes (`{intake_id}.bookrack-extraction.v2.json`) to the
/// kinded form produced by `envelope_filename(kind, intake_id)`.
/// Files already carrying a `book-` or `paper-` prefix are skipped;
/// the operation is idempotent.
///
/// With `dry_run = true` the plan is computed and returned without
/// touching the disk.
pub async fn rename_envelopes(selection: &LibrarySelection, dry_run: bool) -> Result<RenameReport> {
    let cfg = Config::resolve(selection).context("resolve config for envelope rename")?;
    Ok(rename_envelopes_in(
        &cfg.books_dir(),
        &cfg.papers_dir(),
        dry_run,
    ))
}

/// Pure, sync core of [`rename_envelopes`]: scans the two given
/// directories and returns the report. Exposed for tests that drive
/// the rename without going through config resolution.
pub fn rename_envelopes_in(
    books_dir: &std::path::Path,
    papers_dir: &std::path::Path,
    dry_run: bool,
) -> RenameReport {
    let mut report = RenameReport {
        dry_run,
        ..Default::default()
    };
    scan_envelopes(
        books_dir,
        bookrack_core::ItemKind::Book,
        dry_run,
        &mut report,
    );
    scan_envelopes(
        papers_dir,
        bookrack_core::ItemKind::Paper,
        dry_run,
        &mut report,
    );
    report
}

fn scan_envelopes(
    dir: &std::path::Path,
    kind: bookrack_core::ItemKind,
    dry_run: bool,
    report: &mut RenameReport,
) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        // A missing opaque store is a non-event: nothing to migrate.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            report.failures.push(RenameFailure {
                path: dir.display().to_string(),
                error: format!("read dir: {e}"),
            });
            return;
        }
    };
    let mut entries: Vec<std::path::PathBuf> = read
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(bookrack_extract::envelope::ENVELOPE_FILE_SUFFIX))
        })
        .collect();
    // Deterministic order so `--dry-run` and the real run agree.
    entries.sort();

    for from in entries {
        let basename = match from.file_name().and_then(|n| n.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if basename.starts_with("book-") || basename.starts_with("paper-") {
            report.already_prefixed += 1;
            continue;
        }
        let stem = basename
            .strip_suffix(bookrack_extract::envelope::ENVELOPE_FILE_SUFFIX)
            .unwrap_or(basename);
        let intake_id: i64 = match stem.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let to_name = bookrack_extract::envelope_filename(kind, intake_id);
        let to = from.with_file_name(&to_name);

        report.renamed.push(RenameAction {
            kind: kind.as_scope_str().to_string(),
            from: from.display().to_string(),
            to: to.display().to_string(),
        });
        if !dry_run && let Err(e) = std::fs::rename(&from, &to) {
            let last = report.renamed.pop().expect("pushed above");
            report.failures.push(RenameFailure {
                path: last.from,
                error: format!("rename: {e}"),
            });
        }
    }
}

/// Render a [`RenameReport`] to the operator. The text view matches
/// the style of the other doctor outputs (label, value, status); the
/// JSON view emits the report verbatim.
pub fn render_rename_report(report: &RenameReport, json: bool) {
    if json {
        let v = serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string());
        println!("{v}");
        return;
    }
    let mode = if report.dry_run { "(plan)" } else { "" };
    println!(
        "envelope rename {mode}: {} planned, {} already prefixed, {} failed",
        report.renamed.len(),
        report.already_prefixed,
        report.failures.len(),
    );
    for action in &report.renamed {
        let verb = if report.dry_run {
            "would rename"
        } else {
            "renamed"
        };
        println!(
            "  {verb} [{kind}] {from}  ->  {to}",
            kind = action.kind,
            from = action.from,
            to = action.to,
        );
    }
    for failure in &report.failures {
        println!("  FAILED {} ({})", failure.path, failure.error);
    }
}

/// Outcome of a `--backfill-ocr-derivation` run: OCR product intakes
/// whose `derived_from_sha256` was still NULL, recovered from their
/// envelope provenance so `intake list-ocr-pending` stops listing their
/// already-processed sources.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BackfillReport {
    /// True when the plan was computed without writing.
    pub dry_run: bool,
    /// Edges that were (or would be) filled from envelope provenance.
    pub filled: Vec<BackfillAction>,
    /// Rows that could not be backfilled automatically and need a
    /// manual re-OCR: envelope missing, unreadable, or carrying no
    /// derivation hash.
    pub needs_manual: Vec<BackfillFailure>,
}

impl BackfillReport {
    /// True when at least one row needs manual attention.
    pub fn has_failures(&self) -> bool {
        !self.needs_manual.is_empty()
    }
}

/// One OCR intake whose derivation edge was recovered.
#[derive(Debug, Clone, Serialize)]
pub struct BackfillAction {
    /// The OCR product intake id.
    pub intake_id: i64,
    /// The scan PDF hash recovered from the envelope and written onto
    /// the row.
    pub derived_from_sha256: String,
}

/// One OCR intake that could not be backfilled automatically.
#[derive(Debug, Clone, Serialize)]
pub struct BackfillFailure {
    /// The OCR product intake id.
    pub intake_id: i64,
    /// Why the derivation edge could not be recovered.
    pub reason: String,
}

/// Recover the `derived_from_sha256` edge on OCR product intakes that
/// predate the column, reading the parent scan PDF's hash from each
/// intake's envelope provenance. Idempotent: rows whose edge is already
/// set are not revisited (the accessor filters on NULL), and the
/// write-once conflict guard refuses to re-point an existing edge.
///
/// This is an **offline** repair: it opens the catalog for writing,
/// which would race the daemon's exclusive write handle, so the caller
/// must ensure no daemon is serving this library before invoking it.
///
/// With `dry_run = true` the catalog is opened read-only — no migration
/// is applied and no row is written — and the plan is returned. A
/// read-only open of a database still at the pre-column schema fails
/// cleanly rather than silently migrating it.
pub async fn backfill_ocr_derivation(
    selection: &LibrarySelection,
    dry_run: bool,
) -> Result<BackfillReport> {
    let cfg = Config::resolve(selection).context("resolve config for OCR derivation backfill")?;
    // A dry run must not touch the database: the read-only open neither
    // migrates nor writes. The real run opens read-write, which also
    // applies any pending migration as part of the repair.
    let catalog = if dry_run {
        Catalog::open_read_only(&cfg.catalog_db())
            .context("open catalog (read-only) for OCR derivation backfill plan")?
    } else {
        Catalog::open(&cfg.catalog_db()).context("open catalog for OCR derivation backfill")?
    };
    let pending = catalog
        .ocr_intakes_missing_derivation()
        .context("list OCR intakes missing a derivation edge")?;

    let mut report = BackfillReport {
        dry_run,
        ..Default::default()
    };
    for intake in pending {
        let Some(stored_path) = intake.stored_path.as_deref() else {
            report.needs_manual.push(BackfillFailure {
                intake_id: intake.intake_id,
                reason: "no stored envelope path recorded".to_string(),
            });
            continue;
        };
        let envelope = match bookrack_extract::envelope::read_envelope_with_fallback(
            std::path::Path::new(stored_path),
        ) {
            Ok(env) => env,
            Err(e) => {
                report.needs_manual.push(BackfillFailure {
                    intake_id: intake.intake_id,
                    reason: format!("read envelope: {e}"),
                });
                continue;
            }
        };
        let Some(sha) = envelope.extraction.provenance.derived_from_sha256.clone() else {
            report.needs_manual.push(BackfillFailure {
                intake_id: intake.intake_id,
                reason: "envelope provenance carries no derived_from_sha256".to_string(),
            });
            continue;
        };
        if !dry_run
            && let Err(e) =
                catalog.set_derived_from(bookrack_core::ItemKind::Book, intake.intake_id, &sha)
        {
            report.needs_manual.push(BackfillFailure {
                intake_id: intake.intake_id,
                reason: format!("write derivation edge: {e}"),
            });
            continue;
        }
        report.filled.push(BackfillAction {
            intake_id: intake.intake_id,
            derived_from_sha256: sha,
        });
    }
    Ok(report)
}

/// Render a [`BackfillReport`] to the operator, matching the style of
/// [`render_rename_report`]. The JSON view emits the report verbatim.
pub fn render_backfill_report(report: &BackfillReport, json: bool) {
    if json {
        let v = serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string());
        println!("{v}");
        return;
    }
    let mode = if report.dry_run { "(plan)" } else { "" };
    println!(
        "OCR derivation backfill {mode}: {} filled, {} need manual re-OCR",
        report.filled.len(),
        report.needs_manual.len(),
    );
    for action in &report.filled {
        let verb = if report.dry_run {
            "would fill"
        } else {
            "filled"
        };
        println!(
            "  {verb} intake {} -> {}",
            action.intake_id, action.derived_from_sha256,
        );
    }
    for failure in &report.needs_manual {
        println!("  MANUAL intake {} ({})", failure.intake_id, failure.reason);
    }
}

fn push_data_root_row(rows: &mut Vec<Row>, selection: &LibrarySelection) -> Option<Config> {
    match Config::resolve(selection) {
        Ok(cfg) => {
            let value = cfg.data_dir().display().to_string();
            let source = resolution_source_label(cfg.source());
            let identified = cfg
                .library_identification()
                .and_then(library_identification_label)
                .zip(cfg.library())
                .map(|(label, name)| format!("identified as '{name}' by {label}"));
            let status = data_root_status(source, cfg.shadowed_default(), identified.as_deref());
            rows.push(Row {
                label: "data root".to_string(),
                value,
                status,
            });
            Some(cfg)
        }
        Err(ConfigError::MissingDataDir) => {
            let registry_hint = match default_registry_path() {
                Some(p) => format!("run `bookrack init` (writes {})", p.display()),
                None => "run `bookrack init`".to_string(),
            };
            rows.push(Row {
                label: "data root".to_string(),
                value: "(none configured)".to_string(),
                status: Status::Fail {
                    note: registry_hint,
                },
            });
            None
        }
        Err(e) => {
            rows.push(Row {
                label: "data root".to_string(),
                value: "(unresolved)".to_string(),
                status: Status::Fail {
                    note: format!("{e}"),
                },
            });
            None
        }
    }
}

fn data_root_status(
    source: &str,
    shadowed: Option<&ShadowedDefault>,
    identified: Option<&str>,
) -> Status {
    let suffix = identified
        .map(|note| format!("; {note}"))
        .unwrap_or_default();
    match shadowed {
        Some(shadowed) => Status::Warn {
            note: format!(
                "registry default '{}' ({}) is shadowed by {source}; unset it or \
                 pass --library {} to serve the registered library{suffix}",
                shadowed.name,
                shadowed.data_dir.display(),
                shadowed.name,
            ),
        },
        None => Status::Ok {
            note: Some(format!("resolved via {source}{suffix}")),
        },
    }
}

fn push_pdfium_row(rows: &mut Vec<Row>) {
    let filename = pdfium_library_filename();
    let location = locate_pdfium();
    match location.dir {
        Some(dir) => rows.push(Row {
            label: "PDFium library".to_string(),
            value: dir.join(filename).display().to_string(),
            status: Status::Ok { note: None },
        }),
        None => {
            let searched = location
                .probed
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            rows.push(Row {
                label: "PDFium library".to_string(),
                value: format!("(missing) searched {searched}"),
                status: Status::Fail {
                    note: format!(
                        "run `bookrack doctor --install-pdfium` to download \
                         the pinned build, or set BOOKRACK_PDFIUM_LIB to a \
                         directory containing {filename}"
                    ),
                },
            });
        }
    }
}

/// Report the soft `RLIMIT_NOFILE` after attempting the same raise the
/// daemon performs at startup, so the row shows the limit a daemon
/// launched from this environment would actually run with.
fn push_fd_limit_row(rows: &mut Vec<Row>) {
    let label = "fd limit".to_string();
    match crate::rlimit::raise_nofile() {
        Ok(None) => rows.push(Row {
            label,
            value: "unlimited".to_string(),
            status: Status::Ok { note: None },
        }),
        Ok(Some(soft)) if soft >= crate::rlimit::NOFILE_TARGET => rows.push(Row {
            label,
            value: soft.to_string(),
            status: Status::Ok { note: None },
        }),
        Ok(Some(soft)) => rows.push(Row {
            label,
            value: soft.to_string(),
            status: Status::Warn {
                note: format!(
                    "below {}; a large ingest batch may hit `Too many open files`",
                    crate::rlimit::NOFILE_TARGET
                ),
            },
        }),
        Err(e) => rows.push(Row {
            label,
            value: "(unknown)".to_string(),
            status: Status::Warn {
                note: format!("could not raise RLIMIT_NOFILE: {e}"),
            },
        }),
    }
}

fn push_catalog_row(rows: &mut Vec<Row>, cfg: &Config) {
    push_store_row(rows, "catalog.db", &cfg.catalog_db());
}

fn push_corpus_row(rows: &mut Vec<Row>, cfg: &Config) {
    push_store_row(rows, "corpus.db", &cfg.corpus_db());
}

/// Report a database store by filesystem presence only. Opening a handle
/// is deferred to the daemon so doctor never competes with a live
/// session for the exclusive write lock.
fn push_store_row(rows: &mut Vec<Row>, label: &str, path: &std::path::Path) {
    if path.exists() {
        rows.push(Row {
            label: label.to_string(),
            value: path.display().to_string(),
            status: Status::Ok { note: None },
        });
    } else {
        rows.push(Row {
            label: label.to_string(),
            value: "(not initialised)".to_string(),
            status: Status::Warn {
                note: "no books ingested yet; the first `bookrack ingest` creates it".to_string(),
            },
        });
    }
}

/// What probing a registry entry's data root for its identity manifest
/// found. Kept as a value so the drift classification is a pure function
/// a test can drive without a filesystem.
#[derive(Debug, Clone)]
enum ManifestProbe {
    /// The data root does not exist on disk.
    Missing,
    /// The data root exists but carries no identity manifest.
    NoManifest,
    /// The data root exists but its manifest could not be read.
    Unreadable(String),
    /// The manifest loaded; the uuid it records.
    Loaded { uuid: String },
}

/// Compare one registry entry against its on-disk manifest, returning the
/// drift note when they disagree, or `None` when consistent. A name
/// alias and a kind override are legal (04's registration rules), so only
/// a stale uuid cache, a missing root, or an unreadable manifest is
/// flagged — the registry caches the uuid, so a mismatch there is the
/// signal that the cache is stale.
fn registry_entry_issue(entry: &LibraryEntry, probe: &ManifestProbe) -> Option<String> {
    let root = entry.data_dir.display();
    match probe {
        ManifestProbe::Missing => Some(format!(
            "'{}' data root {root} not found (moved, or an unmounted volume)",
            entry.name
        )),
        ManifestProbe::Unreadable(reason) => Some(format!(
            "'{}' identity manifest at {root} is unreadable (corrupt, or an unmounted volume): {reason}",
            entry.name
        )),
        ManifestProbe::NoManifest => entry.uuid.as_ref().map(|_| {
            format!(
                "'{}' has a cached uuid but {root} carries no manifest; re-register to refresh",
                entry.name
            )
        }),
        ManifestProbe::Loaded { uuid } => entry
            .uuid
            .as_deref()
            .filter(|cached| *cached != uuid)
            .map(|cached| {
                format!(
                    "'{}' registry uuid {} is stale; the manifest at {root} records {}. \
                     Re-register to refresh the cache",
                    entry.name,
                    short(cached),
                    short(uuid),
                )
            }),
    }
}

/// Probe a data root for its manifest, mapping the filesystem outcomes to
/// a [`ManifestProbe`].
fn probe_manifest(data_dir: &Path) -> ManifestProbe {
    if !data_dir.exists() {
        return ManifestProbe::Missing;
    }
    match load_manifest(data_dir) {
        Ok(Some(manifest)) => ManifestProbe::Loaded {
            uuid: manifest.uuid,
        },
        Ok(None) => ManifestProbe::NoManifest,
        Err(e) => ManifestProbe::Unreadable(manifest_error_reason(&e)),
    }
}

/// A compact reason string for a manifest read failure.
fn manifest_error_reason(error: &ManifestError) -> String {
    match error {
        ManifestError::Io { .. } => "cannot read the file".to_string(),
        ManifestError::Parse { .. } => "does not parse".to_string(),
        ManifestError::SchemaVersion { found, .. } => {
            format!("schema version {found} is newer than this binary")
        }
        ManifestError::NotALibrary { .. } => "not a bookrack manifest".to_string(),
    }
}

/// What resolving the library registry found. Kept as a value so the
/// two registry-backed sections classify one observation rather than
/// reading the registry once each, and so a test drives them without
/// touching the process environment.
#[derive(Debug, Clone)]
enum RegistryProbe {
    /// No registry is configured, or the file it names does not exist.
    /// Both sections stay silent: the registry is optional and the write
    /// verbs create it on first use, so its absence is a fresh install
    /// rather than a fault.
    Absent,
    /// The registry resolved; the entries it lists, sorted by name.
    Entries(Vec<LibraryEntry>),
    /// The registry file is present but could not be read: its path and
    /// the compact reason.
    Unreadable { path: String, reason: String },
}

/// Map a [`list_libraries`] outcome to a [`RegistryProbe`], keeping
/// "there is no registry" and "the registry cannot be read" apart. A
/// file that does not exist joins [`RegistryProbe::Absent`] whether the
/// environment named it or it resolved as the platform default, so the
/// two states the config layer distinguishes reach the report intact.
fn probe_registry(listed: Result<Option<Vec<LibraryEntry>>, ConfigError>) -> RegistryProbe {
    match listed {
        Ok(Some(entries)) => RegistryProbe::Entries(entries),
        Ok(None) => RegistryProbe::Absent,
        Err(ConfigError::RegistryUnreadable { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            RegistryProbe::Absent
        }
        Err(error) => RegistryProbe::Unreadable {
            path: registry_error_path(&error),
            reason: registry_error_reason(&error),
        },
    }
}

/// The registry path a read failure names, for the row's value column.
/// Falls back to the platform default so the row still points at a
/// file when the error carries no path of its own.
fn registry_error_path(error: &ConfigError) -> String {
    match error {
        ConfigError::RegistryUnreadable { path, .. }
        | ConfigError::RegistryMalformed { path, .. } => path.display().to_string(),
        _ => default_registry_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unknown)".to_string()),
    }
}

/// A compact reason for a registry read failure, in the shape
/// [`manifest_error_reason`] uses for an identity manifest.
fn registry_error_reason(error: &ConfigError) -> String {
    match error {
        ConfigError::RegistryUnreadable { source, .. } => format!("cannot be read: {source}"),
        ConfigError::RegistryMalformed { .. } => "does not parse".to_string(),
        other => other.to_string(),
    }
}

/// Emit the registry–manifest consistency check. One WARN row per
/// drifted entry, or a single OK summary when every entry agrees with its
/// manifest. Skipped entirely when there is no registry or it is empty;
/// a registry that exists but cannot be read is reported as its own WARN
/// row instead of joining that silence.
fn push_registry_consistency_rows(rows: &mut Vec<Row>, probe: &RegistryProbe) {
    let entries = match probe {
        RegistryProbe::Absent => return,
        RegistryProbe::Unreadable { path, reason } => {
            rows.push(Row {
                label: "registry".to_string(),
                value: path.clone(),
                status: Status::Warn {
                    note: format!(
                        "the registry {reason}; library names cannot be resolved until it is \
                         fixed or rebuilt with `bookrack libraries scan --register`"
                    ),
                },
            });
            return;
        }
        RegistryProbe::Entries(entries) => entries,
    };
    if entries.is_empty() {
        return;
    }
    let mut clean = true;
    for entry in entries {
        let probe = probe_manifest(&entry.data_dir);
        if let Some(note) = registry_entry_issue(entry, &probe) {
            clean = false;
            rows.push(Row {
                label: "registry".to_string(),
                value: entry.name.clone(),
                status: Status::Warn { note },
            });
        }
    }
    if clean {
        rows.push(Row {
            label: "registry".to_string(),
            value: format!("{} entries", entries.len()),
            status: Status::Ok {
                note: Some("consistent with their manifests".to_string()),
            },
        });
    }
}

/// The per-user index-profile directory, beside `registry.toml`.
fn index_profile_dir() -> Option<std::path::PathBuf> {
    crate::profile::user_profile_dir()
}

/// The built embed-model/dimension stamp pair for the book pipeline;
/// see [`crate::profile::built_stamps`] for the three-way contract
/// (missing / unreadable / stamped) this passes through.
fn built_stamps(data_dir: &Path) -> Result<Option<(String, u32)>, String> {
    crate::profile::built_stamps(&crate::profile::Pipeline::Books.corpus_db(data_dir))
        .map(|stamps| stamps.and_then(|b| b.embed_pair()))
}

/// Classify one entry's index-profile reference against its resolution
/// outcome and the built stamps. Pure, so a test drives it without a
/// filesystem. `resolved` is `Ok(Some(embed_model, dim, has_errors))`
/// for a valid profile, `Ok(None)` when the name does not resolve, and
/// `Err(reason)` when the file failed to load. `built` is
/// `Ok(Some(pair))` for a stamped index, `Ok(None)` when no index has
/// been built, and `Err(reason)` when the corpus database exists but
/// cannot be opened — the latter is reported instead of being passed
/// off as a clean skip.
fn coherence_issue(
    entry_name: &str,
    profile_name: &str,
    resolved: Result<Option<(String, u32, bool)>, String>,
    built: Result<Option<(String, u32)>, String>,
) -> Option<String> {
    match resolved {
        Err(reason) => Some(format!(
            "'{entry_name}' references index profile '{profile_name}', which failed to load: {reason}"
        )),
        Ok(None) => Some(format!(
            "'{entry_name}' references index profile '{profile_name}', which is not defined"
        )),
        Ok(Some((_, _, true))) => Some(format!(
            "'{entry_name}' references index profile '{profile_name}', which has validation errors \
             (run `bookrack index-profile validate {profile_name}`)"
        )),
        Ok(Some((model, dim, false))) => match built {
            Err(reason) => Some(format!(
                "corpus database for '{entry_name}' cannot be opened ({reason}); coherence with \
                 index profile '{profile_name}' was not checked"
            )),
            Ok(built) => built.and_then(|(built_model, built_dim)| {
                (built_model != model || built_dim != dim).then(|| {
                    format!(
                        "index profile '{profile_name}' for '{entry_name}' declares {model}/{dim} but \
                         the built index is {built_model}/{built_dim}; the daemon will refuse to start"
                    )
                })
            }),
        },
    }
}

/// The profile reference in effect for `entry`, and the sources that
/// disagree with it. Resolved by the same chain the daemon applies —
/// manifest, then `config.toml`, then the entry's own cached copy — so
/// doctor reports on the profile the library actually runs under rather
/// than on a stale cache. Unreadable sources count as absent, matching
/// resolution elsewhere.
fn entry_profile_reference(
    entry: &LibraryEntry,
) -> (Option<(String, ProfileRefOrigin)>, Vec<ProfileRefDrift>) {
    let manifest_ref = load_manifest(&entry.data_dir)
        .ok()
        .flatten()
        .and_then(|m| m.index_profile);
    let effective =
        effective_profile_reference(manifest_ref.as_deref(), entry.index_profile.as_deref());
    let drift = profile_reference_drift(manifest_ref.as_deref(), entry.index_profile.as_deref());
    (effective, drift)
}

/// The drift note for a library whose lower-priority sources still name
/// a profile other than the effective one: a stale copy left by an older
/// write path or a hand edit. Harmless to run under — the effective
/// reference is unambiguous — but worth repairing before the stale name
/// confuses the next reader. `None` when nothing drifted.
fn drift_issue(entry_name: &str, effective: &str, drift: &[ProfileRefDrift]) -> Option<String> {
    if drift.is_empty() {
        return None;
    }
    let stale = drift
        .iter()
        .map(|d| format!("{} names '{}'", d.source.as_str(), d.stale_value))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "'{entry_name}' runs under index profile '{effective}' but {stale}; \
         re-run `bookrack index-profile apply {effective}` to rewrite the stale copies, \
         or `bookrack libraries config {entry_name} --unset index_profile` to clear a \
         stale config.toml declaration"
    ))
}

/// Emit the index-profile coherence check: for every library that
/// references a profile, resolve and validate the reference in effect,
/// compare it against the library's built index stamps, and report any
/// lower-priority source still naming a different profile. One WARN row
/// per problem, or a single OK summary when every referenced profile is
/// coherent. Skipped entirely when no library references a profile; a
/// registry that cannot be read leaves a row saying the check was
/// skipped, so an absent section is never read as a clean one.
fn push_index_profile_coherence_rows(rows: &mut Vec<Row>, probe: &RegistryProbe) {
    push_index_profile_coherence_rows_in(rows, probe, index_profile_dir().as_deref());
}

/// [`push_index_profile_coherence_rows`] with the user profile
/// directory supplied, so a test resolves references against a seeded
/// directory instead of the per-user one.
fn push_index_profile_coherence_rows_in(
    rows: &mut Vec<Row>,
    probe: &RegistryProbe,
    profile_dir: Option<&Path>,
) {
    let entries = match probe {
        RegistryProbe::Absent => return,
        RegistryProbe::Unreadable { .. } => {
            rows.push(Row {
                label: "index-profile".to_string(),
                value: "(skipped)".to_string(),
                status: Status::Warn {
                    note: "the registry could not be read, so profile coherence was not checked"
                        .to_string(),
                },
            });
            return;
        }
        RegistryProbe::Entries(entries) => entries,
    };
    let referencing: Vec<(&LibraryEntry, String, Vec<ProfileRefDrift>)> = entries
        .iter()
        .filter_map(|e| {
            let (effective, drift) = entry_profile_reference(e);
            effective.map(|(name, _)| (e, name, drift))
        })
        .collect();
    if referencing.is_empty() {
        return;
    }
    // The summary counts what the loop below checks — the effective
    // reference, manifest first — not the registry's cached copy, which
    // a manifest-only declaration leaves empty.
    let referenced = referencing.len();
    let mut clean = true;
    for (entry, profile_name, drift) in referencing {
        let profile_name = profile_name.as_str();
        let resolved = match profile_dir {
            Some(dir) => match resolve(Some(dir), profile_name) {
                Ok(Some((profile, _source))) => Ok(Some((
                    profile.embed.model.clone(),
                    profile.embed.dim,
                    has_errors(&validate(&profile, false)),
                ))),
                Ok(None) => Ok(None),
                Err(e) => Err(e.to_string()),
            },
            None => Ok(None),
        };
        let built = built_stamps(&entry.data_dir);
        if let Some(note) = coherence_issue(&entry.name, profile_name, resolved, built) {
            clean = false;
            rows.push(Row {
                label: "index-profile".to_string(),
                value: entry.name.clone(),
                status: Status::Warn { note },
            });
        }
        if let Some(note) = drift_issue(&entry.name, profile_name, &drift) {
            clean = false;
            rows.push(Row {
                label: "index-profile".to_string(),
                value: entry.name.clone(),
                status: Status::Warn { note },
            });
        }
    }
    if clean {
        rows.push(Row {
            label: "index-profile".to_string(),
            value: format!("{referenced} referenced"),
            status: Status::Ok {
                note: Some("coherent with their built indexes".to_string()),
            },
        });
    }
}

/// The first dash-delimited segment of an identifier, for compact display.
fn short(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

fn ollama_url_for_probe(cfg: Option<&Config>) -> String {
    cfg.map(|c| c.ollama_url().to_string())
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string())
}

/// The embed model the Ollama probe should look for, by the resolution
/// chain the daemon applies: the effective index profile, best-effort,
/// then the default. Profile resolution failures fall through silently —
/// the coherence rows report them; the probe only needs a model name to
/// ask about.
fn embed_model_for_probe(cfg: Option<&Config>) -> String {
    cfg.and_then(|c| crate::profile::effective_index_profile(c).ok())
        .flatten()
        .map(|p| p.profile.embed.model)
        .unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string())
}

/// The reranker section: the two artifact rows and the backend row.
/// Hidden entirely when the effective profile enables no reranker
/// stage — the section is a profile option, not a host capability, so
/// doctor does not warn about machinery the library never uses. With
/// an operator URL configured the artifact rows are skipped too: the
/// operator's server is the backend, and a missing managed binary
/// would be noise.
async fn push_reranker_rows(
    rows: &mut Vec<Row>,
    cfg: Option<&Config>,
    supervisor: Option<&crate::rerank_supervisor::RerankSupervisor>,
) {
    let Some(cfg) = cfg else {
        return;
    };
    let Ok(Some(effective)) = crate::profile::effective_index_profile(cfg) else {
        // Unresolvable profile states are the coherence rows' story.
        return;
    };
    let spec = effective.profile.reranker;
    if spec.kind == bookrack_index_profile::RerankerKind::None {
        return;
    }
    let reranker_cfg = bookrack_config::RerankerConfig::resolve(cfg.root_config());
    if let Some(url) = &reranker_cfg.url {
        let health = bookrack_rerank::probe_health(url).await;
        rows.push(url_backend_row(url, &health));
        return;
    }

    rows.push(reranker_binary_row(
        bookrack_config::llama_server_pin::locate_llama_server().path,
    ));
    let model_tag = spec.model.as_deref().unwrap_or_default();
    rows.push(reranker_model_row(
        model_tag,
        bookrack_config::reranker_model_pin::locate_reranker_model(model_tag).path,
    ));
    rows.push(match supervisor {
        Some(supervisor) => {
            supervised_backend_row(&supervisor.state().await, supervisor.restarts())
        }
        None => not_running_backend_row(),
    });
}

fn reranker_binary_row(path: Option<std::path::PathBuf>) -> Row {
    match path {
        Some(path) => Row {
            label: "llama-server binary".to_string(),
            value: path.display().to_string(),
            status: Status::Ok { note: None },
        },
        None => Row {
            label: "llama-server binary".to_string(),
            value: "missing".to_string(),
            status: Status::Fail {
                note: "install with `bookrack doctor --install-reranker`".to_string(),
            },
        },
    }
}

fn reranker_model_row(model_tag: &str, path: Option<std::path::PathBuf>) -> Row {
    match path {
        Some(path) => Row {
            label: "reranker model".to_string(),
            value: format!("{model_tag} ({})", path.display()),
            status: Status::Ok { note: None },
        },
        None => Row {
            label: "reranker model".to_string(),
            value: format!("{model_tag} missing"),
            status: Status::Fail {
                note: "install with `bookrack doctor --install-reranker`".to_string(),
            },
        },
    }
}

/// The backend row when this process holds the supervisor — a running
/// daemon reporting on its own child.
fn supervised_backend_row(state: &crate::rerank_supervisor::SupervisorState, restarts: u32) -> Row {
    use crate::rerank_supervisor::SupervisorState;
    let (value, status) = match state {
        SupervisorState::Ready => (
            "supervised".to_string(),
            Status::Ok {
                note: (restarts > 0).then(|| format!("{restarts} restart(s)")),
            },
        ),
        SupervisorState::Starting => (
            "supervised".to_string(),
            Status::Warn {
                note: "starting".to_string(),
            },
        ),
        SupervisorState::Restarting { attempt, .. } => (
            "supervised".to_string(),
            Status::Warn {
                note: format!("restarting (attempt {attempt}); queries fail until ready"),
            },
        ),
    };
    Row {
        label: "reranker backend".to_string(),
        value,
        status,
    }
}

/// The backend row when no daemon supervises a server: nothing is
/// wrong, there is simply nothing to observe.
fn not_running_backend_row() -> Row {
    Row {
        label: "reranker backend".to_string(),
        value: "not running".to_string(),
        status: Status::Ok {
            note: Some("the daemon supervises llama-server when it runs".to_string()),
        },
    }
}

/// The backend row for the operator-URL mode: the named server is
/// probed directly, whether or not a daemon runs.
fn url_backend_row(url: &str, health: &bookrack_rerank::ServerHealth) -> Row {
    let status = match health {
        bookrack_rerank::ServerHealth::Ready => Status::Ok {
            note: Some("operator-run (reranker.url)".to_string()),
        },
        bookrack_rerank::ServerHealth::Starting => Status::Warn {
            note: "still loading its model".to_string(),
        },
        bookrack_rerank::ServerHealth::Unreachable(detail) => Status::Fail {
            note: format!("{detail} -- fix the server behind reranker.url or unset it"),
        },
    };
    Row {
        label: "reranker backend".to_string(),
        value: url.to_string(),
        status,
    }
}

/// Probe the MCP address and lay the answer out as one row.
///
/// `served` is the address the calling daemon bound, present only when
/// the report is gathered inside a running daemon. Absent, the row
/// reports on the *configured* address instead: with no daemon up, the
/// operator's question is whether the address a daemon would take is
/// free, and the answer is worth having before the start rather than
/// after it.
async fn push_mcp_endpoint_row(rows: &mut Vec<Row>, served: Option<&str>) {
    let (addr, serving) = match served {
        Some(addr) => (addr.to_string(), true),
        None => (bookrack_config::McpConfig::from_env().addr, false),
    };
    // A session running without the surface has no address to probe,
    // and no failure to report either: `--no-mcp` asked for this.
    if addr == "disabled" {
        rows.push(Row {
            label: "MCP endpoint".to_string(),
            value: "disabled".to_string(),
            status: Status::Ok {
                note: Some("session started with --no-mcp".to_string()),
            },
        });
        return;
    }
    let state =
        crate::mcp_endpoint::probe_endpoint(&addr, crate::mcp_endpoint::PROBE_TIMEOUT).await;
    rows.push(mcp_endpoint_row(&addr, serving, state));
}

/// The pure layout half of [`push_mcp_endpoint_row`], separated so
/// every combination can be pinned without a server.
///
/// `serving` says whether a daemon claims to hold this address. It
/// decides the severity, not the facts: silence at an address nobody
/// serves is the ordinary state of a stopped daemon, while silence at
/// the address this daemon reports holding is a broken product
/// surface. A stranger answering is a failure either way — that is
/// the state in which a client following the documented URL reaches
/// somebody else.
fn mcp_endpoint_row(addr: &str, serving: bool, state: McpEndpointState) -> Row {
    let status = match (&state, serving) {
        (McpEndpointState::Serving { version }, _) => Status::Ok {
            note: Some(format!("bookrack {version}")),
        },
        (McpEndpointState::Foreign { .. }, _) => Status::Fail {
            note: "answered by another service -- an agent client here reaches it, not bookrack"
                .to_string(),
        },
        (McpEndpointState::Unreachable, true) => Status::Fail {
            note: format!(
                "no answer within {}s from the address this daemon reports serving",
                crate::mcp_endpoint::PROBE_TIMEOUT.as_secs()
            ),
        },
        (McpEndpointState::Unreachable, false) => Status::Ok {
            note: Some("free -- no daemon is serving it".to_string()),
        },
    };
    Row {
        label: "MCP endpoint".to_string(),
        value: addr.to_string(),
        status,
    }
}

/// Lay the embed-backend judgement out as the two rows the table has
/// always shown. The judgement itself lives in
/// [`crate::backend_probe`]; only the layout is here, so bring-up can
/// reach the same verdict without inheriting a table cell's phrasing.
async fn push_ollama_rows(rows: &mut Vec<Row>, base_url: &str, embed_model: &str) {
    let state = check_embed_backend(base_url, embed_model).await;
    rows.extend(ollama_rows(base_url, embed_model, state));
}

/// The pure layout half of [`push_ollama_rows`], separated so the four
/// outcomes can be pinned without a network.
fn ollama_rows(base_url: &str, embed_model: &str, state: EmbedBackendState) -> [Row; 2] {
    let (daemon_status, model_status) = match state {
        EmbedBackendState::Ready { models } => (
            Status::Ok {
                note: Some(format!("{} model(s) pulled", models.len())),
            },
            Status::Ok { note: None },
        ),
        EmbedBackendState::ModelMissing { model, available } => (
            Status::Ok {
                note: Some(format!("{} model(s) pulled", available.len())),
            },
            Status::Fail {
                // The same fragment the bring-up hint wraps in a
                // sentence. Sharing the whole sentence instead
                // would force one of the two to change shape.
                note: format!("not pulled -- run `{}`", pull_command(&model)),
            },
        ),
        EmbedBackendState::Unreachable => (
            Status::Fail {
                note: format!(
                    "unreachable within {}s -- is Ollama running? install: https://ollama.com",
                    DEFAULT_PROBE_TIMEOUT.as_secs(),
                ),
            },
            Status::Fail {
                note: "skipped: Ollama unreachable".to_string(),
            },
        ),
        EmbedBackendState::ProbeFailed { reason } => (
            Status::Fail { note: reason },
            Status::Fail {
                note: "skipped: Ollama probe failed".to_string(),
            },
        ),
    };
    [
        Row {
            label: "Ollama daemon".to_string(),
            value: base_url.to_string(),
            status: daemon_status,
        },
        Row {
            label: "embed model".to_string(),
            value: embed_model.to_string(),
            status: model_status,
        },
    ]
}

fn resolution_source_label(source: ResolutionSource) -> &'static str {
    match source {
        ResolutionSource::DataDirFlag => "--data-dir flag",
        ResolutionSource::LibraryFlag => "--library flag",
        ResolutionSource::EnvVar => "BOOKRACK_DATA_DIR env",
        ResolutionSource::PortableExeNeighbor => "portable layout",
        ResolutionSource::RegistryDefault => "registry default",
        ResolutionSource::DefaultRegistryDefault => "default registry default",
        ResolutionSource::Explicit => "explicit",
    }
}

/// Render a [`LibraryIdentification`] as the note appended to the data
/// root row, or `None` when there is nothing worth surfacing.
/// `Selected` yields `None`: a registry selection is already conveyed by
/// the resolution source, so only a path-class root claimed after the
/// fact carries a note.
fn library_identification_label(id: LibraryIdentification) -> Option<&'static str> {
    match id {
        LibraryIdentification::Selected => None,
        LibraryIdentification::ManifestUuid => Some("manifest uuid"),
        LibraryIdentification::Path => Some("path"),
    }
}

fn render_text(report: &Report) {
    // Column widths chosen so a typical row fits in 100 columns. Long
    // values still wrap to a single line; the operator sees the noun
    // (the value) before the verdict.
    let label_w = report
        .rows
        .iter()
        .map(|r| r.label.len())
        .max()
        .unwrap_or(0)
        .max(12);
    let value_w = report
        .rows
        .iter()
        .map(|r| r.value.len())
        .max()
        .unwrap_or(0)
        .max(20);
    for row in &report.rows {
        let (tag, note) = render_status(&row.status);
        println!(
            "{label:<lw$}  {value:<vw$}  {tag:<5} {note}",
            label = row.label,
            lw = label_w,
            value = row.value,
            vw = value_w,
            tag = tag,
            note = note,
        );
    }
    println!();
    if report.has_failures() {
        println!(
            "bookrack is not ready. {} problem(s).",
            report.failure_count()
        );
    } else {
        println!("bookrack is ready.");
    }
}

/// Render one status as `(tag, note)` for the text formatter. The note
/// is empty when there is none to print rather than `None`, so the
/// caller can interpolate it unconditionally.
fn render_status(status: &Status) -> (&'static str, String) {
    match status {
        Status::Ok { note } => ("OK", note.clone().unwrap_or_default()),
        Status::Warn { note } => ("WARN", note.clone()),
        Status::Fail { note } => ("FAIL", note.clone()),
    }
}

fn render_json(report: &Report) {
    match serde_json::to_string_pretty(report) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("doctor: failed to serialise report: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use bookrack_core::Explain;

    use super::*;

    /// Every note the two Ollama rows can carry, transcribed from the
    /// table as it stood before the judgement moved out. Pinning the
    /// literals is the point: extracting the judgement must not shift
    /// a single character of what an operator reads.
    #[test]
    fn doctor_rows_are_unchanged_by_the_extraction() {
        let cases = [
            (
                EmbedBackendState::Ready {
                    models: vec!["m".into(), "n".into()],
                },
                Status::Ok {
                    note: Some("2 model(s) pulled".to_string()),
                },
                Status::Ok { note: None },
            ),
            (
                EmbedBackendState::ModelMissing {
                    model: "m".into(),
                    available: vec!["n".into()],
                },
                Status::Ok {
                    note: Some("1 model(s) pulled".to_string()),
                },
                Status::Fail {
                    note: "not pulled -- run `ollama pull m`".to_string(),
                },
            ),
            (
                EmbedBackendState::Unreachable,
                Status::Fail {
                    note: "unreachable within 2s -- is Ollama running? \
                           install: https://ollama.com"
                        .to_string(),
                },
                Status::Fail {
                    note: "skipped: Ollama unreachable".to_string(),
                },
            ),
            (
                EmbedBackendState::ProbeFailed {
                    reason: "Ollama returned a malformed /api/tags response: eof".to_string(),
                },
                Status::Fail {
                    note: "Ollama returned a malformed /api/tags response: eof".to_string(),
                },
                Status::Fail {
                    note: "skipped: Ollama probe failed".to_string(),
                },
            ),
        ];
        for (state, daemon, model) in cases {
            let label = format!("{state:?}");
            let [daemon_row, model_row] = ollama_rows("http://host:11434", "m", state);
            assert_eq!(daemon_row.label, "Ollama daemon", "{label}");
            assert_eq!(daemon_row.value, "http://host:11434", "{label}");
            assert_eq!(
                format!("{:?}", daemon_row.status),
                format!("{daemon:?}"),
                "{label}"
            );
            assert_eq!(model_row.label, "embed model", "{label}");
            assert_eq!(model_row.value, "m", "{label}");
            assert_eq!(
                format!("{:?}", model_row.status),
                format!("{model:?}"),
                "{label}"
            );
        }
    }

    /// Silence means opposite things on the two sides of the
    /// `serving` flag, and a stranger answering means the same thing
    /// on both. Every combination is pinned, because the whole point
    /// of the row is that it does not report a healthy endpoint when
    /// the endpoint is somebody else's.
    #[test]
    fn the_mcp_endpoint_row_grades_each_state_by_whether_a_daemon_claims_the_address() {
        let serving_ok = mcp_endpoint_row(
            "127.0.0.1:8765",
            true,
            McpEndpointState::Serving {
                version: "0.11.0-dev".to_string(),
            },
        );
        assert!(
            matches!(serving_ok.status, Status::Ok { .. }),
            "a served endpoint answering as bookrack is the healthy row: {:?}",
            serving_ok.status
        );
        assert_eq!(serving_ok.value, "127.0.0.1:8765");

        for serving in [true, false] {
            let foreign = mcp_endpoint_row(
                "127.0.0.1:8765",
                serving,
                McpEndpointState::Foreign {
                    evidence: "HTTP 200, body \"nope\"".to_string(),
                },
            );
            assert!(
                matches!(foreign.status, Status::Fail { .. }),
                "a stranger on the address is a failure whether or not a daemon claims it \
                 (serving={serving}): {:?}",
                foreign.status
            );
        }

        let silent_while_serving =
            mcp_endpoint_row("127.0.0.1:8765", true, McpEndpointState::Unreachable);
        assert!(
            matches!(silent_while_serving.status, Status::Fail { .. }),
            "an address this daemon reports serving must answer: {:?}",
            silent_while_serving.status
        );

        let silent_with_no_daemon =
            mcp_endpoint_row("127.0.0.1:8765", false, McpEndpointState::Unreachable);
        assert!(
            matches!(silent_with_no_daemon.status, Status::Ok { .. }),
            "a free address with no daemon running is not a fault: {:?}",
            silent_with_no_daemon.status
        );
    }

    /// The table cell and the bring-up hint share the repair command,
    /// not the sentence around it. Asserting on the shared fragment is
    /// what makes this a same-source check rather than two literals
    /// that happen to agree today.
    #[test]
    fn doctor_and_preflight_share_one_pull_command() {
        let state = EmbedBackendState::ModelMissing {
            model: "wanted-model".to_string(),
            available: Vec::new(),
        };
        let command = pull_command("wanted-model");
        let hint = state.explain().data.hint.expect("hint");
        let [_, model_row] = ollama_rows("http://host:11434", "wanted-model", state);
        let Status::Fail { note } = model_row.status else {
            panic!("a missing model is a FAIL row");
        };
        assert!(note.contains(&command), "{note}");
        assert!(hint.contains(&command), "{hint}");
    }

    fn row(label: &str, value: &str, status: Status) -> Row {
        Row {
            label: label.to_string(),
            value: value.to_string(),
            status,
        }
    }

    fn entry(name: &str, uuid: Option<&str>) -> LibraryEntry {
        LibraryEntry {
            name: name.to_string(),
            data_dir: std::path::PathBuf::from(format!("/roots/{name}")),
            is_default: false,
            kind: bookrack_config::LibraryKind::Prod,
            description: None,
            index_profile: None,
            created_at: None,
            uuid: uuid.map(str::to_string),
        }
    }

    #[test]
    fn reranker_rows_cover_the_four_states() {
        // ok: both artifacts found, supervisor ready.
        let found = reranker_binary_row(Some(std::path::PathBuf::from("/managed/llama-server")));
        assert!(matches!(found.status, Status::Ok { .. }), "{found:?}");
        let model = reranker_model_row(
            "Qwen3-Reranker-0.6B",
            Some(std::path::PathBuf::from("/managed/m.gguf")),
        );
        assert!(model.value.starts_with("Qwen3-Reranker-0.6B"), "{model:?}");
        let ready = supervised_backend_row(&crate::rerank_supervisor::SupervisorState::Ready, 2);
        assert!(
            matches!(&ready.status, Status::Ok { note: Some(n) } if n.contains("2 restart")),
            "{ready:?}"
        );

        // missing: both artifact rows fail with the install hint.
        for missing in [
            reranker_binary_row(None),
            reranker_model_row("Qwen3-Reranker-0.6B", None),
        ] {
            assert!(
                matches!(&missing.status, Status::Fail { note } if note.contains("--install-reranker")),
                "{missing:?}"
            );
        }

        // not running: an offline doctor is not a failure.
        let offline = not_running_backend_row();
        assert_eq!(offline.value, "not running");
        assert!(matches!(offline.status, Status::Ok { .. }), "{offline:?}");

        // restarting: a crash window warns without failing the report.
        let restarting = supervised_backend_row(
            &crate::rerank_supervisor::SupervisorState::Restarting {
                attempt: 3,
                next_delay: std::time::Duration::from_secs(4),
            },
            3,
        );
        assert!(
            matches!(&restarting.status, Status::Warn { note } if note.contains("attempt 3")),
            "{restarting:?}"
        );
    }

    #[test]
    fn url_backend_row_maps_probe_outcomes() {
        use bookrack_rerank::ServerHealth;
        let ready = url_backend_row("http://h:1", &ServerHealth::Ready);
        assert!(
            matches!(&ready.status, Status::Ok { note: Some(n) } if n.contains("operator-run")),
            "{ready:?}"
        );
        let starting = url_backend_row("http://h:1", &ServerHealth::Starting);
        assert!(
            matches!(starting.status, Status::Warn { .. }),
            "{starting:?}"
        );
        let dead = url_backend_row(
            "http://h:1",
            &ServerHealth::Unreachable("connection refused".to_string()),
        );
        assert!(
            matches!(&dead.status, Status::Fail { note } if note.contains("reranker.url")),
            "{dead:?}"
        );
    }

    #[test]
    fn registry_entry_is_clean_when_uuid_matches() {
        let e = entry("alpha", Some("01890a5d-0000-7000-8000-000000000000"));
        let probe = ManifestProbe::Loaded {
            uuid: "01890a5d-0000-7000-8000-000000000000".to_string(),
        };
        assert!(registry_entry_issue(&e, &probe).is_none());
    }

    #[test]
    fn registry_entry_flags_a_stale_uuid_cache() {
        let e = entry("alpha", Some("01890a5d-0000-7000-8000-000000000000"));
        let probe = ManifestProbe::Loaded {
            uuid: "ffffffff-0000-7000-8000-000000000000".to_string(),
        };
        let note = registry_entry_issue(&e, &probe).expect("stale uuid is flagged");
        assert!(note.contains("stale"), "{note}");
    }

    #[test]
    fn registry_entry_name_alias_and_kind_override_are_not_drift() {
        // A name alias is legal: the manifest's birth name may differ
        // from the registry key. Only the uuid cache is compared.
        let e = entry("alias", Some("01890a5d-0000-7000-8000-000000000000"));
        let probe = ManifestProbe::Loaded {
            uuid: "01890a5d-0000-7000-8000-000000000000".to_string(),
        };
        assert!(registry_entry_issue(&e, &probe).is_none());
    }

    #[test]
    fn registry_entry_missing_root_and_unreadable_manifest_differ() {
        let e = entry("alpha", Some("u"));
        let missing = registry_entry_issue(&e, &ManifestProbe::Missing).expect("missing flagged");
        assert!(missing.contains("not found"), "{missing}");
        let unreadable =
            registry_entry_issue(&e, &ManifestProbe::Unreadable("does not parse".to_string()))
                .expect("unreadable flagged");
        assert!(unreadable.contains("unreadable"), "{unreadable}");
    }

    #[test]
    fn registry_entry_no_manifest_flags_only_a_cached_uuid() {
        // A legacy bare root with no cached uuid is consistent; one that
        // caches a uuid but has lost its manifest is not.
        assert!(registry_entry_issue(&entry("legacy", None), &ManifestProbe::NoManifest).is_none());
        assert!(
            registry_entry_issue(&entry("cached", Some("u")), &ManifestProbe::NoManifest).is_some()
        );
    }

    #[test]
    fn an_unreadable_registry_is_reported_by_both_registry_sections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("registry.toml");
        std::fs::write(&path, "this is not = valid = toml").expect("seed a malformed registry");
        let error = bookrack_config::list_libraries_at(&path)
            .expect_err("a malformed registry must not parse");

        let probe = probe_registry(Err(error));
        let RegistryProbe::Unreadable {
            path: shown,
            reason,
        } = &probe
        else {
            panic!("a registry that exists but cannot be read must not probe as absent: {probe:?}");
        };
        assert_eq!(shown, &path.display().to_string());
        assert!(reason.contains("does not parse"), "{reason}");

        let mut rows = Vec::new();
        push_registry_consistency_rows(&mut rows, &probe);
        push_index_profile_coherence_rows(&mut rows, &probe);

        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["registry", "index-profile"], "{rows:?}");
        for row in &rows {
            assert!(
                matches!(row.status, Status::Warn { .. }),
                "an unreadable registry is a warning, not a pass: {row:?}"
            );
        }
        assert!(rows[0].value.contains("registry.toml"), "{:?}", rows[0]);
        let Status::Warn { note } = &rows[1].status else {
            unreachable!()
        };
        assert!(note.contains("could not be read"), "{note}");
    }

    #[test]
    fn the_coherence_summary_counts_the_libraries_it_actually_checked() {
        // The manifest outranks the registry's cached copy, so a library
        // that declares its profile only in the manifest is checked by
        // the loop above. The summary must count the same set, or a
        // library it inspected goes unmentioned in the total.
        let root = tempfile::tempdir().expect("tempdir");
        bookrack_config::set_manifest_index_profile(
            root.path(),
            Some(bookrack_index_profile::PROFILE_QWEN3_06B_DEFAULT),
            bookrack_config::ManifestIdentitySeed {
                name: "declared",
                kind: bookrack_config::LibraryKind::Test,
                description: None,
            },
        )
        .expect("seed a manifest declaring a profile");

        let mut entry = entry("declared", None);
        entry.data_dir = root.path().to_path_buf();
        assert!(
            entry.index_profile.is_none(),
            "the registry cache is deliberately empty",
        );
        let profiles = tempfile::tempdir().expect("tempdir");

        let mut rows = Vec::new();
        push_index_profile_coherence_rows_in(
            &mut rows,
            &RegistryProbe::Entries(vec![entry]),
            Some(profiles.path()),
        );

        // No corpus.db, so the stamp comparison is skipped and the
        // section lands on its clean summary.
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(matches!(rows[0].status, Status::Ok { .. }), "{rows:?}");
        assert_eq!(rows[0].value, "1 referenced");
    }

    #[test]
    fn a_registry_that_is_absent_or_uncreated_leaves_both_sections_silent() {
        // `Ok(None)` is "no registry is configured"; a `NotFound` read
        // failure is a configured path the write verbs have not created
        // yet. Neither is a fault, so neither may produce a row — the
        // reporting above must not be bought by warning on fresh
        // installs.
        let listed: [Result<Option<Vec<LibraryEntry>>, ConfigError>; 2] = [
            Ok(None),
            Err(ConfigError::RegistryUnreadable {
                path: std::path::PathBuf::from("/roots/registry.toml"),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            }),
        ];
        for outcome in listed {
            let probe = probe_registry(outcome);
            assert!(
                matches!(probe, RegistryProbe::Absent),
                "expected an absent registry, got {probe:?}"
            );
            let mut rows = Vec::new();
            push_registry_consistency_rows(&mut rows, &probe);
            push_index_profile_coherence_rows(&mut rows, &probe);
            assert!(rows.is_empty(), "{rows:?}");
        }
    }

    #[test]
    fn coherence_unresolved_and_invalid_and_mismatch_are_flagged() {
        // Unresolved profile.
        assert!(coherence_issue("lib", "p", Ok(None), Ok(None)).is_some());
        // Failed to load.
        assert!(coherence_issue("lib", "p", Err("boom".to_string()), Ok(None)).is_some());
        // Has validation errors.
        assert!(
            coherence_issue("lib", "p", Ok(Some(("m".to_string(), 8, true))), Ok(None)).is_some()
        );
        // Valid and coherent with the built stamps.
        assert!(
            coherence_issue(
                "lib",
                "p",
                Ok(Some(("m".to_string(), 8, false))),
                Ok(Some(("m".to_string(), 8))),
            )
            .is_none()
        );
        // Valid but disagrees with the built stamps.
        let note = coherence_issue(
            "lib",
            "p",
            Ok(Some(("m".to_string(), 8, false))),
            Ok(Some(("other".to_string(), 8))),
        )
        .expect("mismatch flagged");
        assert!(note.contains("refuse to start"), "{note}");
    }

    #[test]
    fn coherence_skips_the_stamp_check_when_the_index_is_unbuilt() {
        // No built stamps (corpus missing, so no index built yet) and a
        // valid profile is not a problem — the check cannot compare.
        assert!(
            coherence_issue("lib", "p", Ok(Some(("m".to_string(), 8, false))), Ok(None)).is_none()
        );
    }

    #[test]
    fn coherence_flags_an_unreadable_corpus_instead_of_skipping() {
        // An existing corpus that cannot be opened is a distinct state
        // from "unbuilt": the check must surface it, not report clean.
        let note = coherence_issue(
            "lib",
            "p",
            Ok(Some(("m".to_string(), 8, false))),
            Err("schema version 99 is newer than this binary".to_string()),
        )
        .expect("unreadable corpus flagged");
        assert!(note.contains("cannot be opened"), "{note}");
        assert!(note.contains("was not checked"), "{note}");
    }

    #[test]
    fn report_failure_summary_counts_only_fail_rows() {
        let report = Report {
            rows: vec![
                row("a", "x", Status::Ok { note: None }),
                row(
                    "b",
                    "y",
                    Status::Warn {
                        note: "soft".to_string(),
                    },
                ),
                row(
                    "c",
                    "z",
                    Status::Fail {
                        note: "hard".to_string(),
                    },
                ),
                row(
                    "d",
                    "w",
                    Status::Fail {
                        note: "hard".to_string(),
                    },
                ),
            ],
        };
        assert!(report.has_failures());
        assert_eq!(report.failure_count(), 2);
    }

    #[test]
    fn report_with_only_ok_rows_passes() {
        let report = Report {
            rows: vec![
                row("a", "x", Status::Ok { note: None }),
                row(
                    "b",
                    "y",
                    Status::Ok {
                        note: Some("hint".to_string()),
                    },
                ),
            ],
        };
        assert!(!report.has_failures());
        assert_eq!(report.failure_count(), 0);
    }

    #[test]
    fn json_serialisation_flattens_status_field() {
        let report = Report {
            rows: vec![row(
                "data root",
                "/abs",
                Status::Ok {
                    note: Some("via flag".to_string()),
                },
            )],
        };
        let serialised = serde_json::to_string(&report).expect("serialises");
        assert!(serialised.contains(r#""label":"data root""#));
        assert!(serialised.contains(r#""status":"ok""#));
        assert!(serialised.contains(r#""note":"via flag""#));
    }

    #[test]
    fn data_root_status_is_ok_when_nothing_is_shadowed() {
        let status = data_root_status("--data-dir flag", None, None);
        match status {
            Status::Ok { note } => {
                assert_eq!(note.as_deref(), Some("resolved via --data-dir flag"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn data_root_status_appends_the_identification_note() {
        let status = data_root_status(
            "BOOKRACK_DATA_DIR env",
            None,
            Some("identified as 'hammer' by manifest uuid"),
        );
        match status {
            Status::Ok { note } => {
                assert_eq!(
                    note.as_deref(),
                    Some(
                        "resolved via BOOKRACK_DATA_DIR env; identified as 'hammer' by manifest uuid"
                    )
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn data_root_status_carries_identification_alongside_a_shadow() {
        let shadowed = ShadowedDefault {
            name: "eval-data".to_string(),
            data_dir: std::path::PathBuf::from("/roots/eval-data"),
        };
        let status = data_root_status(
            "BOOKRACK_DATA_DIR env",
            Some(&shadowed),
            Some("identified as 'hammer' by path"),
        );
        match status {
            Status::Warn { note } => {
                assert!(
                    note.contains("registry default 'eval-data'"),
                    "missing shadow: {note}"
                );
                assert!(
                    note.ends_with("identified as 'hammer' by path"),
                    "missing identification: {note}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn library_identification_label_hides_a_registry_selection() {
        assert_eq!(
            library_identification_label(LibraryIdentification::Selected),
            None
        );
        assert_eq!(
            library_identification_label(LibraryIdentification::ManifestUuid),
            Some("manifest uuid")
        );
        assert_eq!(
            library_identification_label(LibraryIdentification::Path),
            Some("path")
        );
    }

    #[test]
    fn data_root_status_warns_when_a_registry_default_is_shadowed() {
        let shadowed = ShadowedDefault {
            name: "eval-data".to_string(),
            data_dir: std::path::PathBuf::from("/roots/eval-data"),
        };
        let status = data_root_status("BOOKRACK_DATA_DIR env", Some(&shadowed), None);
        match status {
            Status::Warn { note } => {
                assert!(
                    note.contains("registry default 'eval-data' (/roots/eval-data)"),
                    "missing name and path: {note}"
                );
                assert!(
                    note.contains("is shadowed by BOOKRACK_DATA_DIR env"),
                    "missing source: {note}"
                );
                assert!(
                    note.contains("pass --library eval-data"),
                    "missing remedy: {note}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn json_serialisation_omits_note_when_ok_without_note() {
        let report = Report {
            rows: vec![row("a", "x", Status::Ok { note: None })],
        };
        let serialised = serde_json::to_string(&report).expect("serialises");
        assert!(serialised.contains(r#""status":"ok""#));
        assert!(
            !serialised.contains(r#""note""#),
            "note should be elided: {serialised}"
        );
    }

    fn write_legacy_envelope(dir: &std::path::Path, intake_id: i64) -> std::path::PathBuf {
        let path = dir.join(bookrack_extract::envelope_filename_legacy(intake_id));
        std::fs::write(&path, b"{\"schema_version\":2}").expect("seed envelope");
        path
    }

    #[test]
    fn rename_envelopes_dry_run_plan_matches_a_real_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let books = tmp.path().join("books");
        let papers = tmp.path().join("papers");
        std::fs::create_dir_all(&books).unwrap();
        std::fs::create_dir_all(&papers).unwrap();
        write_legacy_envelope(&books, 1);
        write_legacy_envelope(&books, 2);
        write_legacy_envelope(&papers, 1);

        let plan = rename_envelopes_in(&books, &papers, true);
        assert_eq!(plan.renamed.len(), 3);
        assert!(plan.failures.is_empty());

        let applied = rename_envelopes_in(&books, &papers, false);
        let plan_pairs: Vec<_> = plan.renamed.iter().map(|a| (&a.from, &a.to)).collect();
        let applied_pairs: Vec<_> = applied.renamed.iter().map(|a| (&a.from, &a.to)).collect();
        assert_eq!(plan_pairs, applied_pairs);
        assert!(applied.failures.is_empty());

        assert!(
            books
                .join(bookrack_extract::envelope_filename(
                    bookrack_core::ItemKind::Book,
                    1
                ))
                .exists(),
            "book-1 envelope should now exist with the kinded prefix"
        );
        assert!(
            papers
                .join(bookrack_extract::envelope_filename(
                    bookrack_core::ItemKind::Paper,
                    1
                ))
                .exists(),
            "paper-1 envelope should now exist with the kinded prefix"
        );
    }

    #[test]
    fn rename_envelopes_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let books = tmp.path().join("books");
        let papers = tmp.path().join("papers");
        std::fs::create_dir_all(&books).unwrap();
        std::fs::create_dir_all(&papers).unwrap();
        write_legacy_envelope(&books, 7);

        let first = rename_envelopes_in(&books, &papers, false);
        assert_eq!(first.renamed.len(), 1);

        let second = rename_envelopes_in(&books, &papers, false);
        assert!(
            second.renamed.is_empty(),
            "second pass should find nothing to rename"
        );
        assert_eq!(second.already_prefixed, 1);
    }

    #[test]
    fn rename_envelopes_tolerates_missing_opaque_stores() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let books = tmp.path().join("books");
        let papers = tmp.path().join("papers");
        // Neither directory exists.
        let report = rename_envelopes_in(&books, &papers, false);
        assert!(report.renamed.is_empty());
        assert!(report.failures.is_empty());
    }
}
