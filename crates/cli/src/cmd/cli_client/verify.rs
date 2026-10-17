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
use bookrack_runtime::cmd::verify::{PipelineFindings, VerifyReport};
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

/// True when a finding says the library is damaged rather than empty.
/// On either pipeline: a store that did not verify or could not be
/// read, an unreadable vector sidecar, one store missing beside the
/// other, or an intake row whose file is gone. Beside them: a
/// `reference.db` that does not open at this build's schema version. A
/// root with no database, a pipeline never used, a library that never
/// built its vector index, and any amount of churn are not damage —
/// not built is not broken.
pub fn is_damaged(report: &VerifyReport) -> bool {
    if report.not_initialised {
        return false;
    }
    pipeline_is_damaged(&report.books)
        || pipeline_is_damaged(&report.papers)
        || report.reference_schema_error.is_some()
}

/// One pipeline's share of [`is_damaged`]. Both stores missing is a
/// pipeline never used, which the inequality leaves alone.
fn pipeline_is_damaged(findings: &PipelineFindings) -> bool {
    findings.catalog_schema_error.is_some()
        || findings.corpus_schema_error.is_some()
        || findings.intake_scan_error.is_some()
        || findings.vectors_meta_error.is_some()
        || findings.catalog_missing != findings.corpus_missing
        || findings
            .missing_intake_files
            .as_ref()
            .is_some_and(|missing| !missing.is_empty())
}

/// The names one pipeline's stores and files are drawn under.
struct Labels {
    pipeline: &'static str,
    catalog: &'static str,
    corpus: &'static str,
    vectors: &'static str,
    intake_root: &'static str,
}

const BOOKS: Labels = Labels {
    pipeline: "books",
    catalog: "catalog.db",
    corpus: "corpus.db",
    vectors: "vectors",
    intake_root: "books/",
};

const PAPERS: Labels = Labels {
    pipeline: "papers",
    catalog: "papers_catalog.db",
    corpus: "papers_corpus.db",
    vectors: "paper vectors",
    intake_root: "papers/",
};

/// Draw the report for a reader. The report itself decides what
/// landed; this only translates. `library` is the name the invocation
/// selected, drawn as a heading because the report carries none. The
/// book side is always drawn; the paper side and `reference.db` only
/// when the library holds them.
pub fn render(report: &VerifyReport, library: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(name) = library {
        let _ = writeln!(out, "library: {name}");
        let _ = writeln!(out);
    }
    if report.not_initialised {
        let _ = writeln!(out, "data directory not initialised yet.");
        let _ = writeln!(out, "  none of its databases is on disk;");
        let _ = writeln!(
            out,
            "  run `bookrack ingest <path>` to create them, then verify again."
        );
        return out;
    }
    push_pipeline(&mut out, &report.books, &BOOKS);
    if !report.papers.absent() || has_vectors(&report.papers) {
        let _ = writeln!(out);
        push_pipeline(&mut out, &report.papers, &PAPERS);
    }
    if !report.reference_missing {
        let _ = writeln!(out);
        let _ = writeln!(out, "reference.db:");
        if report.reference_schema_ok {
            let _ = writeln!(out, "  schema version: ok");
        } else if let Some(err) = &report.reference_schema_error {
            let _ = writeln!(out, "  schema version: FAILED");
            push_reason(&mut out, err);
        }
    }
    out
}

/// Draw one pipeline: a single line when neither store is on disk,
/// otherwise each store with the intake findings under its catalog;
/// then the vector sidecar when it reported anything.
fn push_pipeline(out: &mut String, findings: &PipelineFindings, labels: &Labels) {
    if findings.absent() {
        let _ = writeln!(out, "{}:", labels.pipeline);
        let _ = writeln!(
            out,
            "  not in use (neither {} nor {} on disk)",
            labels.catalog, labels.corpus
        );
    } else {
        push_store(
            out,
            labels.catalog,
            labels.corpus,
            findings.catalog_missing,
            findings.catalog_schema_ok,
            findings.catalog_schema_error.as_deref(),
        );
        if let Some(err) = &findings.intake_scan_error {
            let _ = writeln!(out, "  intakes:        unreadable");
            push_reason(out, err);
        }
        if let Some(n) = findings.intake_count {
            let _ = writeln!(out, "  intakes:        {n}");
        }
        if let Some(missing) = &findings.missing_intake_files {
            if missing.is_empty() {
                let _ = writeln!(
                    out,
                    "  intake files:   every stored_path is present on disk"
                );
            } else {
                let _ = writeln!(
                    out,
                    "  intake files:   {} missing under {}:",
                    missing.len(),
                    labels.intake_root
                );
                for id in missing {
                    let _ = writeln!(out, "    intake {id}");
                }
            }
        }

        let _ = writeln!(out);
        push_store(
            out,
            labels.corpus,
            labels.catalog,
            findings.corpus_missing,
            findings.corpus_schema_ok,
            findings.corpus_schema_error.as_deref(),
        );
    }

    if has_vectors(findings) {
        let _ = writeln!(out);
        let _ = writeln!(out, "{}:", labels.vectors);
        if let Some(err) = &findings.vectors_meta_error {
            let _ = writeln!(out, "  meta:            FAILED");
            push_reason(out, err);
        }
        if let Some(n) = findings.vectors_built_at_chunk_count {
            let _ = writeln!(out, "  chunks_at_build: {n}");
        }
        if let Some(n) = findings.vectors_churn {
            let _ = writeln!(out, "  churn:           {n}");
        }
    }
}

/// Draw one store's heading and its schema line. A missing store is
/// drawn against `sibling`, which [`push_pipeline`] only reaches when
/// the sibling is on disk.
fn push_store(
    out: &mut String,
    name: &str,
    sibling: &str,
    missing: bool,
    schema_ok: bool,
    schema_error: Option<&str>,
) {
    let _ = writeln!(out, "{name}:");
    if missing {
        let _ = writeln!(out, "  missing on disk ({sibling} is present)");
    } else if schema_ok {
        let _ = writeln!(out, "  schema:         ok");
    } else if let Some(err) = schema_error {
        let _ = writeln!(out, "  schema:         FAILED");
        push_reason(out, err);
    }
}

/// True when the vector sidecar reported anything: counts from a
/// built index, or the reason it could not be read.
fn has_vectors(findings: &PipelineFindings) -> bool {
    findings.vectors_built_at_chunk_count.is_some()
        || findings.vectors_churn.is_some()
        || findings.vectors_meta_error.is_some()
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

    /// A pipeline whose two stores read back, holding one intake whose
    /// file is in place, with no vector index.
    fn whole_pipeline() -> PipelineFindings {
        PipelineFindings {
            catalog_schema_ok: true,
            corpus_schema_ok: true,
            intake_count: Some(1),
            missing_intake_files: Some(vec![]),
            ..PipelineFindings::default()
        }
    }

    /// A pipeline with neither store on disk.
    fn unused_pipeline() -> PipelineFindings {
        PipelineFindings {
            catalog_missing: true,
            corpus_missing: true,
            ..PipelineFindings::default()
        }
    }

    /// A library with both book stores, one intake, its file in place,
    /// no vector index, no paper stores, and no reference store.
    fn whole() -> VerifyReport {
        VerifyReport {
            books: whole_pipeline(),
            papers: unused_pipeline(),
            reference_missing: true,
            ..VerifyReport::default()
        }
    }

    /// A whole library whose book or paper side is `findings`; the
    /// paper side of the book case is whole too, so either side alone
    /// decides the verdict.
    fn with_side(side: &str, findings: PipelineFindings) -> VerifyReport {
        match side {
            "books" => VerifyReport {
                books: findings,
                papers: whole_pipeline(),
                ..whole()
            },
            "papers" => VerifyReport {
                papers: findings,
                ..whole()
            },
            other => panic!("no such side: {other}"),
        }
    }

    const SIDES: [&str; 2] = ["books", "papers"];

    #[test]
    fn a_whole_library_and_an_uninitialised_root_are_not_damage() {
        assert!(!is_damaged(&whole()));
        for side in SIDES {
            assert!(!is_damaged(&with_side(side, whole_pipeline())), "{side}");
        }
        assert!(!is_damaged(&VerifyReport {
            not_initialised: true,
            books: unused_pipeline(),
            papers: unused_pipeline(),
            reference_missing: true,
            ..VerifyReport::default()
        }));
    }

    #[test]
    fn a_pipeline_never_used_is_not_damage() {
        let papers_only = VerifyReport {
            books: unused_pipeline(),
            papers: whole_pipeline(),
            ..whole()
        };
        assert!(!is_damaged(&papers_only));
        let reference_only = VerifyReport {
            books: unused_pipeline(),
            reference_missing: false,
            reference_schema_ok: true,
            ..whole()
        };
        assert!(!is_damaged(&reference_only));
    }

    #[test]
    fn every_error_field_is_damage_on_either_side() {
        let reason = Some("boom".to_string());
        for side in SIDES {
            let cases: [(&str, PipelineFindings); 4] = [
                (
                    "catalog_schema_error",
                    PipelineFindings {
                        catalog_schema_ok: false,
                        catalog_schema_error: reason.clone(),
                        ..whole_pipeline()
                    },
                ),
                (
                    "corpus_schema_error",
                    PipelineFindings {
                        corpus_schema_ok: false,
                        corpus_schema_error: reason.clone(),
                        ..whole_pipeline()
                    },
                ),
                (
                    "intake_scan_error",
                    PipelineFindings {
                        intake_count: None,
                        missing_intake_files: None,
                        intake_scan_error: reason.clone(),
                        ..whole_pipeline()
                    },
                ),
                (
                    "vectors_meta_error",
                    PipelineFindings {
                        vectors_meta_error: reason.clone(),
                        ..whole_pipeline()
                    },
                ),
            ];
            for (field, findings) in cases {
                assert!(
                    is_damaged(&with_side(side, findings)),
                    "{side}: {field} set must read as damage"
                );
            }
        }
    }

    #[test]
    fn half_a_pipeline_is_damage_on_either_side() {
        for side in SIDES {
            assert!(
                is_damaged(&with_side(
                    side,
                    PipelineFindings {
                        catalog_missing: true,
                        catalog_schema_ok: false,
                        intake_count: None,
                        missing_intake_files: None,
                        ..whole_pipeline()
                    }
                )),
                "{side}: catalog missing beside the corpus"
            );
            assert!(
                is_damaged(&with_side(
                    side,
                    PipelineFindings {
                        corpus_missing: true,
                        corpus_schema_ok: false,
                        ..whole_pipeline()
                    }
                )),
                "{side}: corpus missing beside the catalog"
            );
        }
    }

    #[test]
    fn a_missing_intake_file_is_damage_on_either_side_and_an_empty_list_is_not() {
        for side in SIDES {
            assert!(
                is_damaged(&with_side(
                    side,
                    PipelineFindings {
                        missing_intake_files: Some(vec![7]),
                        ..whole_pipeline()
                    }
                )),
                "{side}"
            );
            assert!(
                !is_damaged(&with_side(
                    side,
                    PipelineFindings {
                        missing_intake_files: Some(vec![]),
                        ..whole_pipeline()
                    }
                )),
                "{side}"
            );
        }
    }

    #[test]
    fn an_unbuilt_or_churned_index_is_not_damage() {
        for side in SIDES {
            assert!(
                !is_damaged(&with_side(
                    side,
                    PipelineFindings {
                        vectors_built_at_chunk_count: Some(10),
                        vectors_churn: Some(10_000),
                        ..whole_pipeline()
                    }
                )),
                "{side}"
            );
        }
    }

    #[test]
    fn an_unreadable_reference_store_is_damage_and_a_readable_one_is_not() {
        assert!(is_damaged(&VerifyReport {
            reference_missing: false,
            reference_schema_error: Some("reference.db schema is behind this build".into()),
            ..whole()
        }));
        assert!(!is_damaged(&VerifyReport {
            reference_missing: false,
            reference_schema_ok: true,
            ..whole()
        }));
    }

    #[test]
    fn the_report_names_the_missing_files_and_the_unreadable_reason() {
        let text = render(
            &VerifyReport {
                books: PipelineFindings {
                    missing_intake_files: Some(vec![3, 9]),
                    ..whole_pipeline()
                },
                ..whole()
            },
            Some("alpha"),
        );
        assert!(text.starts_with("library: alpha\n"), "{text}");
        assert!(text.contains("2 missing under books/"), "{text}");
        assert!(text.contains("    intake 3\n    intake 9\n"), "{text}");

        let text = render(
            &VerifyReport {
                books: PipelineFindings {
                    intake_count: None,
                    missing_intake_files: None,
                    intake_scan_error: Some(
                        "catalog database error: page 7 is not a b-tree".into(),
                    ),
                    ..whole_pipeline()
                },
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

    #[test]
    fn a_books_only_report_draws_no_paper_or_reference_section() {
        let text = render(&whole(), None);
        assert!(text.starts_with("catalog.db:\n"), "{text}");
        assert!(!text.contains("papers"), "{text}");
        assert!(!text.contains("reference.db"), "{text}");
    }

    #[test]
    fn a_papers_only_report_says_the_book_side_is_unused() {
        let text = render(
            &VerifyReport {
                books: unused_pipeline(),
                papers: whole_pipeline(),
                ..whole()
            },
            None,
        );
        assert!(text.starts_with("books:\n  not in use"), "{text}");
        assert!(
            !text.contains("is present)"),
            "the report claims a store that is not on disk: {text}"
        );
        assert!(
            text.contains("papers_catalog.db:\n  schema:         ok\n"),
            "{text}"
        );
        assert!(
            text.contains("papers_corpus.db:\n  schema:         ok\n"),
            "{text}"
        );
    }

    #[test]
    fn a_reference_only_report_draws_the_reference_store_and_nothing_else() {
        let text = render(
            &VerifyReport {
                books: unused_pipeline(),
                reference_missing: false,
                reference_schema_ok: true,
                ..whole()
            },
            None,
        );
        assert!(text.starts_with("books:\n  not in use"), "{text}");
        assert!(!text.contains("is present)"), "{text}");
        assert!(!text.contains("papers"), "{text}");
        assert!(
            text.contains("reference.db:\n  schema version: ok\n"),
            "{text}"
        );

        let text = render(
            &VerifyReport {
                books: unused_pipeline(),
                reference_missing: false,
                reference_schema_error: Some(
                    "reference.db schema is behind this build: found user_version 0, expects 1"
                        .into(),
                ),
                ..whole()
            },
            None,
        );
        assert!(
            text.contains(
                "reference.db:\n  schema version: FAILED\n    reference.db schema is behind"
            ),
            "{text}"
        );
    }

    #[test]
    fn half_a_paper_pipeline_names_its_own_sibling() {
        let text = render(
            &with_side(
                "papers",
                PipelineFindings {
                    catalog_missing: true,
                    catalog_schema_ok: false,
                    intake_count: None,
                    missing_intake_files: None,
                    ..whole_pipeline()
                },
            ),
            None,
        );
        assert!(
            text.contains("papers_catalog.db:\n  missing on disk (papers_corpus.db is present)\n"),
            "{text}"
        );
        assert!(!text.contains("(corpus.db is present)"), "{text}");

        let text = render(
            &with_side(
                "papers",
                PipelineFindings {
                    corpus_missing: true,
                    corpus_schema_ok: false,
                    ..whole_pipeline()
                },
            ),
            None,
        );
        assert!(
            text.contains("papers_corpus.db:\n  missing on disk (papers_catalog.db is present)\n"),
            "{text}"
        );
        assert!(!text.contains("(catalog.db is present)"), "{text}");
    }

    #[test]
    fn a_missing_paper_file_is_drawn_under_the_paper_section() {
        let text = render(
            &with_side(
                "papers",
                PipelineFindings {
                    missing_intake_files: Some(vec![4]),
                    ..whole_pipeline()
                },
            ),
            None,
        );
        let papers_at = text
            .find("papers_catalog.db:")
            .unwrap_or_else(|| panic!("no paper section: {text}"));
        let finding_at = text
            .find("1 missing under papers/:\n    intake 4\n")
            .unwrap_or_else(|| panic!("the missing paper is not named: {text}"));
        assert!(finding_at > papers_at, "{text}");
        assert!(
            text[..papers_at].contains("every stored_path is present on disk"),
            "{text}"
        );
    }
}
