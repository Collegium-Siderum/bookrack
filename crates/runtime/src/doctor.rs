// SPDX-License-Identifier: Apache-2.0

//! `bookrack doctor`: one-screen health check of an install.
//!
//! Each environment expectation — a resolved data root, the on-disk
//! presence of each database store, a loadable PDFium library, a
//! reachable Ollama daemon carrying the configured embed model —
//! becomes one row in a fixed three-column table. A row is `OK`,
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

use anyhow::{Context, Result};
use bookrack_config::{
    Config, ConfigError, DEFAULT_EMBED_MODEL, DEFAULT_OLLAMA_URL, EMBED_MODEL_ENV,
    LibrarySelection, ResolutionSource, default_registry_path, pdfium_lib_dir,
};
use bookrack_embed::{DEFAULT_PROBE_TIMEOUT, ProbeReport, probe_ollama};
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
/// gathers every check, renders, and sets a non-zero exit code on any
/// FAIL by `bail!`ing.
pub async fn run(selection: &LibrarySelection, json: bool) -> Result<()> {
    let report = gather(selection).await;
    if json {
        render_json(&report);
    } else {
        render_text(&report);
    }
    if report.has_failures() {
        anyhow::bail!(
            "bookrack is not ready: {} problem(s)",
            report.failure_count()
        );
    }
    Ok(())
}

/// Render a [`Report`] previously returned by the control-plane
/// `doctor.gather` RPC. The CLI-side `bookrack doctor` client calls
/// this to keep the text/JSON output identical between the
/// daemon-running and daemon-not-running paths.
pub fn render_value(value: &serde_json::Value, json: bool) -> Result<()> {
    let report: Report =
        serde_json::from_value(value.clone()).context("decode doctor.gather response")?;
    if json {
        render_json(&report);
    } else {
        render_text(&report);
    }
    if report.has_failures() {
        anyhow::bail!(
            "bookrack is not ready: {} problem(s)",
            report.failure_count()
        );
    }
    Ok(())
}

/// Build a [`Report`] for the given selection. Pure over its inputs in
/// the sense that every observation is fresh — there is no in-process
/// cache to invalidate between successive calls.
pub async fn gather(selection: &LibrarySelection) -> Report {
    let mut rows = Vec::new();

    let cfg = push_data_root_row(&mut rows, selection);
    push_pdfium_row(&mut rows);
    if let Some(cfg) = &cfg {
        push_catalog_row(&mut rows, cfg);
        push_corpus_row(&mut rows, cfg);
    }
    let ollama_url = ollama_url_for_probe(cfg.as_ref());
    let embed_model = embed_model_for_probe(cfg.as_ref());
    push_ollama_rows(&mut rows, &ollama_url, &embed_model).await;

    Report { rows }
}

fn push_data_root_row(rows: &mut Vec<Row>, selection: &LibrarySelection) -> Option<Config> {
    match Config::resolve(selection) {
        Ok(cfg) => {
            let label = "data root";
            let value = cfg.data_dir().display().to_string();
            let source = resolution_source_label(cfg.source());
            rows.push(Row {
                label: label.to_string(),
                value,
                status: Status::Ok {
                    note: Some(format!("resolved via {source}")),
                },
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

fn push_pdfium_row(rows: &mut Vec<Row>) {
    let dir = pdfium_lib_dir();
    let filename = pdfium_filename();
    let path = dir.join(filename);
    if path.is_file() {
        rows.push(Row {
            label: "PDFium library".to_string(),
            value: path.display().to_string(),
            status: Status::Ok { note: None },
        });
    } else {
        rows.push(Row {
            label: "PDFium library".to_string(),
            value: format!("(missing) expected {}", path.display()),
            status: Status::Fail {
                note: format!(
                    "drop {filename} next to the binary, \
                     or set BOOKRACK_PDFIUM_LIB to its directory"
                ),
            },
        });
    }
}

/// Platform-conventional filename of the PDFium dynamic library. The
/// adapter loads `pdfium_lib_dir().join(this)`.
fn pdfium_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
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

    #[test]
    fn pdfium_filename_is_platform_specific() {
        let name = pdfium_filename();
        if cfg!(target_os = "windows") {
            assert_eq!(name, "pdfium.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(name, "libpdfium.dylib");
        } else {
            assert_eq!(name, "libpdfium.so");
        }
    }
}
