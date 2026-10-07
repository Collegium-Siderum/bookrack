// SPDX-License-Identifier: Apache-2.0

//! `bookrack verify` — control-plane wrapper that judges the report.
//!
//! The daemon's `verify.run` returns findings and no verdict. This
//! module fetches them, draws the report — text by default, the raw
//! result under `--json` — and then classifies the findings with
//! [`is_damaged`]: one that says the library is damaged raises
//! [`BookrackCliError::VerifyUnhealthy`], so the binary exits 1. The
//! report is drawn before the verdict is raised, so the exit code adds
//! no line of its own.

use std::fmt::Write as _;
use std::path::PathBuf;

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::library_param;
use bookrack_cli::render::ctx;
use bookrack_runtime::cmd::verify::VerifyReport;
use eyre::{Context, Result};
use serde_json::Value;

use super::helpers;

pub async fn run(runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let value = helpers::dispatch(&client, "verify.run", Value::Null).await?;
    let report: VerifyReport =
        serde_json::from_value(value.clone()).context("decode verify.run response")?;
    if ctx().is_json() {
        helpers::print_value(&value);
    } else if !ctx().is_quiet() {
        print!("{}", render(&report, library_param::selected()));
    }
    if is_damaged(&report) {
        return Err(BookrackCliError::VerifyUnhealthy.into());
    }
    Ok(())
}

/// True when a finding says the library is damaged rather than empty:
/// a store that did not verify or could not be read, an unreadable
/// vector sidecar, one store missing beside the other, or an intake
/// row whose file is gone. A root with neither store, a library that
/// never built its vector index, and any amount of churn are not
/// damage — not built is not broken.
pub fn is_damaged(report: &VerifyReport) -> bool {
    if report.not_initialised {
        return false;
    }
    report.catalog_schema_error.is_some()
        || report.corpus_schema_error.is_some()
        || report.intake_scan_error.is_some()
        || report.vectors_meta_error.is_some()
        || report.catalog_missing != report.corpus_missing
        || report
            .missing_intake_files
            .as_ref()
            .is_some_and(|missing| !missing.is_empty())
}

/// Draw the report for a reader. The report itself decides what
/// landed; this only translates. `library` is the name the invocation
/// selected, drawn as a heading because the report carries none.
pub fn render(report: &VerifyReport, library: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(name) = library {
        let _ = writeln!(out, "library: {name}");
        let _ = writeln!(out);
    }
    if report.not_initialised {
        let _ = writeln!(out, "data directory not initialised yet.");
        let _ = writeln!(out, "  neither catalog.db nor corpus.db on disk;");
        let _ = writeln!(
            out,
            "  run `bookrack ingest <path>` to create them, then verify again."
        );
        return out;
    }
    let _ = writeln!(out, "catalog.db:");
    if report.catalog_missing {
        let _ = writeln!(out, "  missing on disk (corpus.db is present)");
    } else if report.catalog_schema_ok {
        let _ = writeln!(out, "  schema:         ok");
    } else if let Some(err) = &report.catalog_schema_error {
        let _ = writeln!(out, "  schema:         FAILED");
        push_reason(&mut out, err);
    }
    if let Some(err) = &report.intake_scan_error {
        let _ = writeln!(out, "  intakes:        unreadable");
        push_reason(&mut out, err);
    }
    if let Some(n) = report.intake_count {
        let _ = writeln!(out, "  intakes:        {n}");
    }
    if let Some(missing) = &report.missing_intake_files {
        if missing.is_empty() {
            let _ = writeln!(
                out,
                "  intake files:   every stored_path is present on disk"
            );
        } else {
            let _ = writeln!(
                out,
                "  intake files:   {} missing under books/:",
                missing.len()
            );
            for id in missing {
                let _ = writeln!(out, "    intake {id}");
            }
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "corpus.db:");
    if report.corpus_missing {
        let _ = writeln!(out, "  missing on disk (catalog.db is present)");
    } else if report.corpus_schema_ok {
        let _ = writeln!(out, "  schema:         ok");
    } else if let Some(err) = &report.corpus_schema_error {
        let _ = writeln!(out, "  schema:         FAILED");
        push_reason(&mut out, err);
    }

    if report.vectors_built_at_chunk_count.is_some()
        || report.vectors_churn.is_some()
        || report.vectors_meta_error.is_some()
    {
        let _ = writeln!(out);
        let _ = writeln!(out, "vectors:");
        if let Some(err) = &report.vectors_meta_error {
            let _ = writeln!(out, "  meta:            FAILED");
            push_reason(&mut out, err);
        }
        if let Some(n) = report.vectors_built_at_chunk_count {
            let _ = writeln!(out, "  chunks_at_build: {n}");
        }
        if let Some(n) = report.vectors_churn {
            let _ = writeln!(out, "  churn:           {n}");
        }
    }
    out
}

/// Indent a flattened reason under the line it explains, one line per
/// line of the reason.
fn push_reason(out: &mut String, reason: &str) {
    for line in reason.lines() {
        let _ = writeln!(out, "    {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library with both stores, one intake, its file in place, and
    /// no vector index.
    fn whole() -> VerifyReport {
        VerifyReport {
            catalog_schema_ok: true,
            corpus_schema_ok: true,
            intake_count: Some(1),
            missing_intake_files: Some(vec![]),
            ..VerifyReport::default()
        }
    }

    #[test]
    fn a_whole_library_and_an_uninitialised_root_are_not_damage() {
        assert!(!is_damaged(&whole()));
        assert!(!is_damaged(&VerifyReport {
            not_initialised: true,
            catalog_missing: true,
            corpus_missing: true,
            ..VerifyReport::default()
        }));
    }

    #[test]
    fn every_error_field_is_damage() {
        let reason = Some("boom".to_string());
        let cases: [(&str, VerifyReport); 4] = [
            (
                "catalog_schema_error",
                VerifyReport {
                    catalog_schema_ok: false,
                    catalog_schema_error: reason.clone(),
                    ..whole()
                },
            ),
            (
                "corpus_schema_error",
                VerifyReport {
                    corpus_schema_ok: false,
                    corpus_schema_error: reason.clone(),
                    ..whole()
                },
            ),
            (
                "intake_scan_error",
                VerifyReport {
                    intake_count: None,
                    missing_intake_files: None,
                    intake_scan_error: reason.clone(),
                    ..whole()
                },
            ),
            (
                "vectors_meta_error",
                VerifyReport {
                    vectors_meta_error: reason.clone(),
                    ..whole()
                },
            ),
        ];
        for (field, report) in cases {
            assert!(is_damaged(&report), "{field} set must read as damage");
        }
    }

    #[test]
    fn half_a_library_is_damage() {
        assert!(is_damaged(&VerifyReport {
            catalog_missing: true,
            catalog_schema_ok: false,
            intake_count: None,
            missing_intake_files: None,
            ..whole()
        }));
        assert!(is_damaged(&VerifyReport {
            corpus_missing: true,
            corpus_schema_ok: false,
            ..whole()
        }));
    }

    #[test]
    fn a_missing_intake_file_is_damage_and_an_empty_list_is_not() {
        assert!(is_damaged(&VerifyReport {
            missing_intake_files: Some(vec![7]),
            ..whole()
        }));
        assert!(!is_damaged(&VerifyReport {
            missing_intake_files: Some(vec![]),
            ..whole()
        }));
    }

    #[test]
    fn an_unbuilt_or_churned_index_is_not_damage() {
        assert!(!is_damaged(&VerifyReport {
            vectors_built_at_chunk_count: Some(10),
            vectors_churn: Some(10_000),
            ..whole()
        }));
    }

    #[test]
    fn the_report_names_the_missing_files_and_the_unreadable_reason() {
        let text = render(
            &VerifyReport {
                missing_intake_files: Some(vec![3, 9]),
                ..whole()
            },
            Some("alpha"),
        );
        assert!(text.starts_with("library: alpha\n"), "{text}");
        assert!(text.contains("2 missing under books/"), "{text}");
        assert!(text.contains("    intake 3\n    intake 9\n"), "{text}");

        let text = render(
            &VerifyReport {
                intake_count: None,
                missing_intake_files: None,
                intake_scan_error: Some("catalog database error: page 7 is not a b-tree".into()),
                ..whole()
            },
            None,
        );
        assert!(!text.starts_with("library:"), "{text}");
        assert!(text.contains("intakes:        unreadable"), "{text}");
        assert!(
            text.contains("    catalog database error: page 7"),
            "{text}"
        );
    }
}
