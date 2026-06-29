// SPDX-License-Identifier: Apache-2.0

//! Project a [`PaperReport`] onto a [`NewNodePaperAudit`] row.
//!
//! The catalog side table holds one wide row per audit; the schema and
//! column lists live in `bookrack-catalog::node_paper_audit`. This
//! module is the one place that turns the in-memory report into that
//! row, so the writer in `lib.rs` stays mechanical and the projection
//! is unit-testable on its own.
//!
//! Mapping rules:
//!
//! - `grade_<field>`: read `report.fields.get(<key>).map(|f| f.grade)`.
//!   A missing entry collapses to [`PaperFieldGrade::Missing`].
//! - `flag_<token>`: `1` if `report.cross_field_flags` or any field's
//!   `flags` list emits the flag at least once; `0` otherwise.
//! - `verdict` / `confidence`: the report's tokens.

use bookrack_catalog::{FLAG_COLUMNS, GRADE_COLUMNS, NewNodePaperAudit};

use super::report::{PaperFieldGrade, PaperFlag, PaperReport};

/// Build one [`NewNodePaperAudit`] row from a [`PaperReport`].
pub fn paper_report_to_audit_row(
    report: &PaperReport,
    intake_id: i64,
    scope: &str,
    profile_name: &str,
    csl_type: Option<&str>,
    audited_at: &str,
    extractor_version: &str,
) -> NewNodePaperAudit {
    let mut grades: [String; GRADE_COLUMNS.len()] = Default::default();
    for (i, (_, field_key)) in GRADE_COLUMNS.iter().enumerate() {
        let grade = report
            .fields
            .get(field_key)
            .map(|f| f.grade)
            .unwrap_or(PaperFieldGrade::Missing);
        grades[i] = grade.as_token().to_string();
    }

    let mut hits: [bool; FLAG_COLUMNS.len()] = [false; FLAG_COLUMNS.len()];
    for flag in &report.cross_field_flags {
        mark(&mut hits, *flag);
    }
    for field in report.fields.values() {
        for flag in &field.flags {
            mark(&mut hits, *flag);
        }
    }
    let mut flags: [u8; FLAG_COLUMNS.len()] = [0; FLAG_COLUMNS.len()];
    for (i, hit) in hits.iter().enumerate() {
        flags[i] = u8::from(*hit);
    }

    NewNodePaperAudit {
        intake_id,
        scope: scope.to_string(),
        profile_name: profile_name.to_string(),
        verdict: report.verdict.as_token().to_string(),
        confidence: report.confidence.as_token().to_string(),
        csl_type: csl_type.map(str::to_string),
        audited_at: audited_at.to_string(),
        extractor_version: extractor_version.to_string(),
        grades,
        flags,
        pipeline_run_id: None,
    }
}

fn mark(hits: &mut [bool], flag: PaperFlag) {
    let token = flag.as_token();
    for (i, col) in FLAG_COLUMNS.iter().enumerate() {
        if col.strip_prefix("flag_") == Some(token) {
            hits[i] = true;
            return;
        }
    }
    debug_assert!(false, "no node_paper_audit column for flag token {token}");
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::audit::report::{PaperConfidence, PaperFieldReport, PaperVerdict};

    fn report_with_one_field(field: &'static str, grade: PaperFieldGrade) -> PaperReport {
        let mut fields = BTreeMap::new();
        fields.insert(field, PaperFieldReport::new(grade));
        PaperReport {
            fields,
            verdict: PaperVerdict::Clean,
            confidence: PaperConfidence::High,
            cross_field_flags: Vec::new(),
        }
    }

    fn row(report: &PaperReport) -> NewNodePaperAudit {
        paper_report_to_audit_row(
            report,
            7,
            "paper",
            "default",
            Some("article-journal"),
            "2026-06-28T10:00:00Z",
            "0.0.0-test",
        )
    }

    #[test]
    fn flag_columns_cover_every_paper_flag_variant() {
        assert_eq!(FLAG_COLUMNS.len(), PaperFlag::ALL.len());
        for flag in PaperFlag::ALL {
            let column = format!("flag_{}", flag.as_token());
            assert!(
                FLAG_COLUMNS.contains(&column.as_str()),
                "{column} missing from FLAG_COLUMNS",
            );
        }
    }

    #[test]
    fn grade_columns_cover_every_audited_field() {
        // Every grade column's field-key matches a key the audit
        // emits. The audit's nine graders feed exactly these keys.
        let expected = [
            "title",
            "year",
            "doi",
            "arxiv",
            "issn",
            "container",
            "abstract",
            "author",
            "language",
        ];
        let actual: Vec<&str> = GRADE_COLUMNS.iter().map(|(_, k)| *k).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn an_absent_field_grades_as_missing() {
        let report = report_with_one_field("title", PaperFieldGrade::Strong);
        let row = row(&report);
        // index 0 is title, index 1 is year.
        assert_eq!(row.grades[0], "strong");
        assert_eq!(row.grades[1], "missing");
    }

    #[test]
    fn flags_from_cross_field_and_per_field_merge_idempotently() {
        let mut fields = BTreeMap::new();
        let mut title = PaperFieldReport::new(PaperFieldGrade::Weak);
        title.push_flag(PaperFlag::PlaceholderValue);
        fields.insert("title", title);
        let report = PaperReport {
            fields,
            verdict: PaperVerdict::NeedsWork,
            confidence: PaperConfidence::Low,
            cross_field_flags: vec![PaperFlag::NoStableIdentifier],
        };
        let row = row(&report);
        // Find the relevant column positions.
        let placeholder_idx = FLAG_COLUMNS
            .iter()
            .position(|c| *c == "flag_placeholder_value")
            .unwrap();
        let no_stable_idx = FLAG_COLUMNS
            .iter()
            .position(|c| *c == "flag_no_stable_identifier")
            .unwrap();
        let doi_idx = FLAG_COLUMNS
            .iter()
            .position(|c| *c == "flag_doi_invalid_format")
            .unwrap();
        assert_eq!(row.flags[placeholder_idx], 1);
        assert_eq!(row.flags[no_stable_idx], 1);
        assert_eq!(row.flags[doi_idx], 0);
        assert_eq!(row.verdict, "needs_work");
        assert_eq!(row.confidence, "low");
    }

    #[test]
    fn header_columns_are_filled_from_arguments() {
        let report = report_with_one_field("title", PaperFieldGrade::Strong);
        let row = row(&report);
        assert_eq!(row.intake_id, 7);
        assert_eq!(row.scope, "paper");
        assert_eq!(row.profile_name, "default");
        assert_eq!(row.csl_type.as_deref(), Some("article-journal"));
        assert_eq!(row.audited_at, "2026-06-28T10:00:00Z");
        assert_eq!(row.extractor_version, "0.0.0-test");
    }
}
