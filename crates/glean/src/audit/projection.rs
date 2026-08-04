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
//! - `profile_name` / `profile_fingerprint` / `profile_toggle_summary`:
//!   derived from the effective profile. A fingerprint or summary that
//!   fails to compute is logged and written as NULL rather than
//!   blocking the audit row.

use bookrack_catalog::{FLAG_COLUMNS, GRADE_COLUMNS, NewNodePaperAudit};
use bookrack_extract::CslType;

use super::profile::PaperAuditProfile;
use super::report::{PaperFieldGrade, PaperFlag, PaperReport};

/// Map a [`CslType`] to its CSL string, the form the `csl_type`
/// column and `node_publication_attrs` both store.
///
/// It lives beside the projection because the row and the attrs write
/// are its two callers, and a second spelling of this mapping is how
/// the column and the matrix behind a verdict would drift apart.
pub(crate) fn csl_type_token(t: CslType) -> &'static str {
    match t {
        CslType::ArticleJournal => "article-journal",
        CslType::PaperConference => "paper-conference",
        CslType::Book => "book",
        CslType::Chapter => "chapter",
        CslType::Thesis => "thesis",
        CslType::Report => "report",
        CslType::Webpage => "webpage",
    }
}

/// Build one [`NewNodePaperAudit`] row from a [`PaperReport`].
#[allow(clippy::too_many_arguments)]
pub fn paper_report_to_audit_row(
    report: &PaperReport,
    intake_id: i64,
    scope: &str,
    profile: &PaperAuditProfile,
    csl_type: Option<&str>,
    audited_at: &str,
    extractor_version: &str,
    pipeline_run_id: Option<&str>,
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

    let profile_fingerprint = match bookrack_audit_profile::profile_fingerprint(profile) {
        Ok(fp) => Some(fp),
        Err(error) => {
            tracing::warn!(%error, "paper audit: failed to fingerprint the profile");
            None
        }
    };
    let profile_toggle_summary = match bookrack_audit_profile::profile_toggle_summary(profile) {
        Ok(summary) => Some(summary),
        Err(error) => {
            tracing::warn!(%error, "paper audit: failed to summarize the profile toggles");
            None
        }
    };

    NewNodePaperAudit {
        intake_id,
        scope: scope.to_string(),
        profile_name: profile.name.clone(),
        verdict: report.verdict.as_token().to_string(),
        confidence: report.confidence.as_token().to_string(),
        csl_type: csl_type.map(str::to_string),
        audited_at: audited_at.to_string(),
        extractor_version: extractor_version.to_string(),
        grades,
        flags,
        pipeline_run_id: pipeline_run_id.map(str::to_string),
        profile_fingerprint,
        profile_toggle_summary,
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
            csl_type: Some(CslType::ArticleJournal),
        }
    }

    fn row(report: &PaperReport) -> NewNodePaperAudit {
        paper_report_to_audit_row(
            report,
            7,
            "paper",
            &PaperAuditProfile::default_profile(),
            Some("article-journal"),
            "2026-06-28T10:00:00Z",
            "0.0.0-test",
            None,
        )
    }

    #[test]
    fn a_pipeline_run_id_flows_through_to_the_audit_row() {
        let report = report_with_one_field("title", PaperFieldGrade::Strong);
        let with_id = paper_report_to_audit_row(
            &report,
            7,
            "paper",
            &PaperAuditProfile::default_profile(),
            Some("article-journal"),
            "2026-06-28T10:00:00Z",
            "0.0.0-test",
            Some("distill_build-2026-06-28T10:00:00Z-deadbeef"),
        );
        assert_eq!(
            with_id.pipeline_run_id.as_deref(),
            Some("distill_build-2026-06-28T10:00:00Z-deadbeef")
        );
        // The same call without an id leaves the column NULL.
        assert_eq!(row(&report).pipeline_run_id, None);
    }

    #[test]
    fn the_audit_row_carries_profile_name_fingerprint_and_summary() {
        let report = report_with_one_field("title", PaperFieldGrade::Strong);
        let row = row(&report);
        assert_eq!(row.profile_name, "default");
        let fp = row.profile_fingerprint.expect("fingerprint present");
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        let summary = row.profile_toggle_summary.expect("summary present");
        assert!(summary.contains(r#""name":"identifier.require_any""#));
        assert!(summary.contains(r#""name":"abstract.required""#));
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

    /// Run the real audit over a paper whose nine audited fields are
    /// all populated, and return its report.
    fn fully_populated_report() -> PaperReport {
        use bookrack_catalog::{Catalog, NewIntake, NewPublicationAttrs};
        use bookrack_core::ItemKind;
        use bookrack_extract::{
            Biblio, Contributor, ContributorRole, Provenance, TextLayerQuality,
        };

        use crate::audit::data::PaperAuditData;
        use crate::audit::signals::{PaperAuditInput, audit_paper};

        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("cafebabe".to_string()).format("pdf".to_string()),
            )
            .expect("register");
        let mut attrs = NewPublicationAttrs::new(intake.intake().intake_id, ItemKind::Paper);
        attrs.title = Some("Synthetic Findings in Test Spaces".to_string());
        attrs.year = Some("2019".to_string());
        attrs.publisher = Some("Journal of Synthetic Results Press".to_string());
        attrs.doi = Some("10.18653/v1/n19-1423".to_string());
        attrs.arxiv_id = Some("2401.12345".to_string());
        attrs.issn = Some("0378-5955".to_string());
        attrs.container_title = Some("Journal of Synthetic Results".to_string());
        attrs.abstract_text = Some(
            "A synthetic abstract long enough to clear the minimum length \
             the default profile requires for the abstract field."
                .to_string(),
        );
        attrs.language = Some("en".to_string());
        catalog.upsert_publication_attrs(&attrs).expect("upsert");
        let effective = catalog
            .effective_publication_attrs(intake.intake().intake_id, ItemKind::Paper)
            .expect("effective");

        let biblio = Biblio {
            title: None,
            subtitle: None,
            publisher: None,
            year: None,
            year_raw: None,
            isbn: None,
            series: None,
            language: None,
            contributors: vec![Contributor {
                name: "Alex Sample".to_string(),
                role: ContributorRole::Author,
                family: None,
                given: None,
                orcid: None,
            }],
            doi: None,
            arxiv_id: None,
            issn: None,
            container_title: None,
            abstract_text: None,
            csl_type: None,
        };
        let provenance = Provenance {
            adapter: "pdf".to_string(),
            extractor_version: 1,
            text_layer_quality: TextLayerQuality::Usable,
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks: Vec::new(),
        };
        let input = PaperAuditInput {
            biblio: &biblio,
            provenance: &provenance,
            effective: &effective,
            body_sample: "The quick brown fox jumps over the lazy dog.",
            source_stem: Some("paper-0001"),
        };
        audit_paper(
            &input,
            &PaperAuditProfile::default_profile(),
            &PaperAuditData::empty(),
        )
    }

    #[test]
    fn grade_columns_cover_every_audited_field() {
        // The set of field keys a real audit emits must equal the set
        // GRADE_COLUMNS projects from, so neither side can drift.
        // `publisher` is the one deliberate exception: graded for the
        // roll-up (a thesis requires its institution) but carried only
        // in the report JSON, with no grade_* column of its own.
        const UNPROJECTED: &[&str] = &["publisher"];
        let report = fully_populated_report();
        for key in UNPROJECTED {
            assert!(
                report.fields.contains_key(key),
                "{key} must still be graded; prune it from UNPROJECTED otherwise",
            );
        }
        let mut emitted: Vec<&str> = report
            .fields
            .keys()
            .copied()
            .filter(|key| !UNPROJECTED.contains(key))
            .collect();
        let mut projected: Vec<&str> = GRADE_COLUMNS.iter().map(|(_, key)| *key).collect();
        emitted.sort_unstable();
        projected.sort_unstable();
        assert_eq!(projected, emitted);
    }

    #[test]
    fn a_fully_populated_paper_grades_no_column_missing() {
        let report = fully_populated_report();
        let row = row(&report);
        for ((col, _), grade) in GRADE_COLUMNS.iter().zip(row.grades.iter()) {
            assert_ne!(
                grade.as_str(),
                "missing",
                "{col} must not grade missing when every audited field is populated",
            );
        }
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
            csl_type: Some(CslType::ArticleJournal),
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
