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
//! The store rows deliberately stop at `path.exists()`. Opening the
//! catalog or corpus would race the daemon's exclusive write lock and
//! could deadlock or corrupt a running session; deeper introspection
//! lives behind the REPL `status` command instead.
//!
//! The command runs **before** `Config::resolve`, so an unconfigured
//! install still produces a row stating that — rather than the resolver
//! short-circuiting the very diagnosis the user needs.

use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, ConfigError, DEFAULT_EMBED_MODEL, DEFAULT_OLLAMA_URL, EMBED_MODEL_ENV,
    LibrarySelection, ResolutionSource, ShadowedDefault, default_registry_path, locate_pdfium,
    pdfium_library_filename,
};
use bookrack_embed::{DEFAULT_PROBE_TIMEOUT, ProbeReport, probe_ollama};
use eyre::{Context, Result};
use serde::Serialize;

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
    let mut rows = Vec::new();

    let cfg = push_data_root_row(&mut rows, selection);
    push_pdfium_row(&mut rows);
    push_fd_limit_row(&mut rows);
    if let Some(cfg) = &cfg {
        push_catalog_row(&mut rows, cfg);
        push_corpus_row(&mut rows, cfg);
    }
    let ollama_url = ollama_url_for_probe(cfg.as_ref());
    let embed_model = embed_model_for_probe(cfg.as_ref());
    push_ollama_rows(&mut rows, &ollama_url, &embed_model).await;

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
            let status = data_root_status(source, cfg.shadowed_default());
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

/// Build the `data root` row's status. A clean resolution is `Ok` with a
/// "resolved via <source>" note; a path-class root that eclipses a
/// registry default is a `Warn` naming the shadowed default and how to
/// serve it. Pure over its inputs so the decision can be tested without
/// mutating the process environment.
fn data_root_status(source: &str, shadowed: Option<&ShadowedDefault>) -> Status {
    match shadowed {
        Some(shadowed) => Status::Warn {
            note: format!(
                "registry default '{}' ({}) is shadowed by {source}; unset it or \
                 pass --library {} to serve the registered library",
                shadowed.name,
                shadowed.data_dir.display(),
                shadowed.name,
            ),
        },
        None => Status::Ok {
            note: Some(format!("resolved via {source}")),
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

fn ollama_url_for_probe(cfg: Option<&Config>) -> String {
    cfg.map(|c| c.ollama_url().to_string())
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string())
}

fn embed_model_for_probe(cfg: Option<&Config>) -> String {
    cfg.and_then(|c| c.root_config().embed_model.clone())
        .or_else(|| std::env::var(EMBED_MODEL_ENV).ok())
        .unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string())
}

async fn push_ollama_rows(rows: &mut Vec<Row>, base_url: &str, embed_model: &str) {
    let probe = probe_ollama(base_url).await;
    match probe {
        Ok(report) if report.reachable => {
            push_ollama_reachable_rows(rows, base_url, embed_model, &report);
        }
        Ok(_) => {
            rows.push(Row {
                label: "Ollama daemon".to_string(),
                value: base_url.to_string(),
                status: Status::Fail {
                    note: format!(
                        "unreachable within {}s -- is Ollama running? install: https://ollama.com",
                        DEFAULT_PROBE_TIMEOUT.as_secs(),
                    ),
                },
            });
            rows.push(Row {
                label: "embed model".to_string(),
                value: embed_model.to_string(),
                status: Status::Fail {
                    note: "skipped: Ollama unreachable".to_string(),
                },
            });
        }
        Err(e) => {
            rows.push(Row {
                label: "Ollama daemon".to_string(),
                value: base_url.to_string(),
                status: Status::Fail {
                    note: format!("{e}"),
                },
            });
            rows.push(Row {
                label: "embed model".to_string(),
                value: embed_model.to_string(),
                status: Status::Fail {
                    note: "skipped: Ollama probe failed".to_string(),
                },
            });
        }
    }
}

fn push_ollama_reachable_rows(
    rows: &mut Vec<Row>,
    base_url: &str,
    embed_model: &str,
    probe: &ProbeReport,
) {
    rows.push(Row {
        label: "Ollama daemon".to_string(),
        value: base_url.to_string(),
        status: Status::Ok {
            note: Some(format!("{} model(s) pulled", probe.models.len())),
        },
    });
    if probe.models.iter().any(|m| m == embed_model) {
        rows.push(Row {
            label: "embed model".to_string(),
            value: embed_model.to_string(),
            status: Status::Ok { note: None },
        });
    } else {
        rows.push(Row {
            label: "embed model".to_string(),
            value: embed_model.to_string(),
            status: Status::Fail {
                note: format!("not pulled -- run `ollama pull {embed_model}`"),
            },
        });
    }
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
    use super::*;

    fn row(label: &str, value: &str, status: Status) -> Row {
        Row {
            label: label.to_string(),
            value: value.to_string(),
            status,
        }
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
        let status = data_root_status("--data-dir flag", None);
        match status {
            Status::Ok { note } => {
                assert_eq!(note.as_deref(), Some("resolved via --data-dir flag"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn data_root_status_warns_when_a_registry_default_is_shadowed() {
        let shadowed = ShadowedDefault {
            name: "eval-data".to_string(),
            data_dir: std::path::PathBuf::from("/roots/eval-data"),
        };
        let status = data_root_status("BOOKRACK_DATA_DIR env", Some(&shadowed));
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
