// SPDX-License-Identifier: Apache-2.0

//! Offline paper-side metadata re-audit: re-run the audit against the
//! current effective metadata, using the extraction cached in the
//! paper's intake-store envelope.
//!
//! No source file is re-extracted and nothing bibliographic is
//! written: the base attrs, the contributors, and the review status
//! all stay as they are. What is written is everything the judgement
//! produced — the `confidence` / `audit_verdict` rollup on
//! `node_publication_attrs`, the `node_paper_audit` projection row,
//! the report JSON in the review row's `notes`, and one pipeline-audit
//! row — so after correcting fields through the metadata write
//! surface, every read surface catches up with the corrections
//! instead of reporting the glean-time outcome forever.
//!
//! The projection row carries no `pipeline_run_id`: a per-item
//! re-audit stays out of the `pipeline_runs` registry, which records
//! whole-library passes, so its judgement belongs to no run.
//!
//! [`build_report`] exposes the same computation with no write at
//! all: it returns the full [`PaperReport`] for read surfaces that
//! need the per-field grades, flags, and hints rather than the two-
//! scalar rollup.

use std::path::Path;
use std::time::Instant;

use bookrack_catalog::{Catalog, Intake};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_extract::envelope::{self, EnvelopeError};

use crate::audit::projection::csl_type_token;
use crate::audit::{
    PaperAuditData, PaperAuditInput, PaperAuditProfile, PaperReport, PaperVerdict, audit_paper,
    paper_report_to_audit_row,
};
use crate::{GleanError, Result, audit_as, paper_body_sample};

/// What one [`reaudit_paper`] call computed and stored.
#[derive(Debug, Clone)]
pub struct ReauditOutcome {
    pub intake_id: i64,
    /// The stored verdict before this re-audit, if any.
    pub previous_verdict: Option<String>,
    /// The stored confidence before this re-audit, if any.
    pub previous_confidence: Option<String>,
    /// The verdict this re-audit computed and stored.
    pub verdict: String,
    /// The confidence this re-audit computed and stored.
    pub confidence: String,
}

/// Re-run the paper-side metadata audit for one intake from its
/// cached extraction, writing back everything the judgement produced:
/// the `confidence` / `audit_verdict` rollup on
/// `node_publication_attrs`, the whole `node_paper_audit` projection
/// row, and the report JSON in the review row's `notes`.
///
/// The audit grades the *effective* metadata — base attrs merged
/// with any overrides — against the same extraction-time signals
/// the glean audit used, read back from the envelope. Returns
/// [`GleanError::MissingEnvelope`] when the intake has no readable
/// envelope and [`GleanError::EnvelopeMismatch`] when the envelope
/// belongs to a different source file.
///
/// The base attrs, the contributors, and the review *status* stay as
/// they are. The notes refresh goes through
/// [`Catalog::update_review_notes`] rather than an upsert precisely so
/// a re-audit cannot walk an `approved` row back to `pending`.
///
/// The projection row's `pipeline_run_id` is always NULL: a per-item
/// re-audit does not open a `pipeline_runs` row, and neither reusing
/// the run that first judged the paper — the judgement is not that
/// run's — nor minting an id no run registry holds would be true.
///
/// A failed write here propagates, unlike the glean path, where an
/// audit-row failure is a warning that must not roll back an ingest.
/// This verb's entire product is the recomputed judgement: reporting
/// success while the read surfaces still show the old one is the
/// exact disagreement it exists to end.
pub fn reaudit_paper(
    catalog: &Catalog,
    intake_id: i64,
    profile: &PaperAuditProfile,
    data: &PaperAuditData,
) -> Result<ReauditOutcome> {
    let intake = catalog
        .intake_by_id(intake_id)?
        .ok_or(GleanError::UnknownIntake(intake_id))?;

    let work_node_id = PartitionIdx::new(intake_id).root().get();
    let started = Instant::now();
    let run_id = maintenance_run_id("reaudit");

    let previous = catalog.publication_attrs(intake_id, ItemKind::Paper)?;
    let previous_verdict = previous.as_ref().and_then(|a| a.audit_verdict.clone());
    let previous_confidence = previous.as_ref().and_then(|a| a.confidence.clone());

    let PaperAudited {
        report,
        extractor_version,
    } = report_for_intake(catalog, &intake, profile, data)?;

    let confidence = report.confidence.as_token().to_string();
    let verdict = report.verdict.as_token().to_string();
    catalog.update_audit_rollup(intake_id, ItemKind::Paper, &confidence, &verdict)?;

    let audited_at = catalog.now_iso()?;
    let row = paper_report_to_audit_row(
        &report,
        intake_id,
        ItemKind::Paper.as_scope_str(),
        profile,
        report.csl_type.map(csl_type_token),
        &audited_at,
        &extractor_version,
        None,
    );
    catalog.upsert_node_paper_audit(&row)?;
    catalog.update_review_notes(intake_id, ItemKind::Paper, &report.to_json())?;

    let outcome = match report.verdict {
        PaperVerdict::Clean => "ok",
        PaperVerdict::NeedsWork => "partial",
    };
    let metric = format!(
        r#"{{"verdict":"{}","confidence":"{}","fields":{}}}"#,
        verdict,
        confidence,
        report.fields.len(),
    );
    audit_as(
        catalog,
        "reaudit",
        &run_id,
        &intake.source_sha256,
        Some(work_node_id),
        "metadata",
        "audit",
        outcome,
        started,
        Some(metric),
        None,
    );

    Ok(ReauditOutcome {
        intake_id,
        previous_verdict,
        previous_confidence,
        verdict,
        confidence,
    })
}

/// Rebuild the audit report for one paper from its cached
/// extraction, with no write-back: the rollup, the review row, and
/// the pipeline trail all stay as they are.
pub fn build_report(
    catalog: &Catalog,
    intake_id: i64,
    profile: &PaperAuditProfile,
    data: &PaperAuditData,
) -> Result<PaperReport> {
    let intake = catalog
        .intake_by_id(intake_id)?
        .ok_or(GleanError::UnknownIntake(intake_id))?;
    Ok(report_for_intake(catalog, &intake, profile, data)?.report)
}

/// One audit of one intake, with the provenance the projection row
/// needs alongside the report.
struct PaperAudited {
    report: PaperReport,
    /// The extractor version recorded in the envelope this audit read,
    /// not the running binary's: the judgement is about that
    /// extraction, and the column says which one.
    extractor_version: String,
}

/// Audit one intake's effective metadata against its cached
/// extraction envelope: read and verify the envelope, assemble the
/// [`PaperAuditInput`], and run the audit.
fn report_for_intake(
    catalog: &Catalog,
    intake: &Intake,
    profile: &PaperAuditProfile,
    data: &PaperAuditData,
) -> Result<PaperAudited> {
    let intake_id = intake.intake_id;
    let stored_path = intake
        .stored_path
        .as_deref()
        .ok_or(GleanError::MissingEnvelope(intake_id))?;
    let envelope = match envelope::read_envelope_with_fallback(Path::new(stored_path)) {
        Ok(env) => env,
        Err(EnvelopeError::Io(_)) => return Err(GleanError::MissingEnvelope(intake_id)),
        Err(e) => return Err(e.into()),
    };
    if envelope.source_sha256 != intake.source_sha256 {
        return Err(GleanError::EnvelopeMismatch(intake_id));
    }

    let effective = catalog.effective_publication_attrs(intake_id, ItemKind::Paper)?;
    let body_sample = paper_body_sample(&envelope.extraction.blocks);
    let source_stem = intake
        .original_path
        .as_deref()
        .and_then(|p| Path::new(p).file_stem())
        .map(|s| s.to_string_lossy().into_owned());
    let input = PaperAuditInput {
        biblio: &envelope.extraction.biblio,
        provenance: &envelope.extraction.provenance,
        effective: &effective,
        body_sample: &body_sample,
        source_stem: source_stem.as_deref(),
    };
    Ok(PaperAudited {
        report: audit_paper(&input, profile, data),
        extractor_version: envelope.extraction.provenance.extractor_version.to_string(),
    })
}

/// One run id ties every audit row from a maintenance operation
/// together. The `glean-{op}-` prefix distinguishes paper-side
/// reaudit rows from ingest's `ingest-{op}-` prefix when a mixed log
/// is inspected.
///
/// This is `item_pipeline_audit`'s own namespace, not the
/// `pipeline_runs` registry: a per-item reaudit recomputes one
/// rollup for one intake and stays out of `bookrack runs list`,
/// which registers whole-library passes.
fn maintenance_run_id(op: &str) -> String {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    format!("glean-{op}-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::{NewIntake, NewOverride, NewPublicationAttrs};
    use bookrack_extract::envelope::{envelope_filename, write_envelope};
    use bookrack_extract::{
        Biblio, Block, BlockKind, ContributorRole, CslType, Extraction, Provenance,
        TextLayerQuality, Toc,
    };
    use bookrack_extract::{Contributor, SkippedUnit};
    use tempfile::TempDir;

    fn sample_extraction(doi: Option<&str>) -> Extraction {
        Extraction {
            biblio: Biblio {
                title: Some("Synthetic Findings in Test Spaces".to_string()),
                subtitle: None,
                publisher: None,
                year: Some(2020),
                year_raw: Some("2020".to_string()),
                isbn: None,
                series: None,
                language: Some("en".to_string()),
                contributors: vec![Contributor {
                    name: "First Author".to_string(),
                    role: ContributorRole::Author,
                    family: Some("Author".to_string()),
                    given: Some("First".to_string()),
                    orcid: None,
                }],
                doi: doi.map(|s| s.to_string()),
                arxiv_id: Some("0000.00001".to_string()),
                issn: None,
                container_title: Some("Proceedings of the Synthetic Conference".to_string()),
                abstract_text: Some(
                    "This synthetic abstract describes a deliberately fictional study \
                     of test spaces. We introduce a placeholder method, evaluate it on \
                     invented data, and report results that exist only to give the \
                     extraction pipeline a realistically shaped abstract to carry \
                     through its stages."
                        .to_string(),
                ),
                csl_type: Some(CslType::PaperConference),
            },
            blocks: vec![Block {
                kind: BlockKind::Body,
                text: "Body sample for language signal".to_string(),
                source_unit: 0,
                style: None,
            }],
            toc: Toc::default(),
            provenance: Provenance {
                adapter: "pdf".to_string(),
                extractor_version: 1,
                text_layer_quality: TextLayerQuality::Usable,
                skipped_units: Vec::<SkippedUnit>::new(),
                derived_from_sha256: None,
                partial_pages: None,
                source_of_structure: None,
                fallbacks: Vec::new(),
            },
        }
    }

    fn seed(catalog: &mut Catalog, dir: &Path, doi: Option<&str>) -> (i64, std::path::PathBuf) {
        let extraction = sample_extraction(doi);
        let sha = "deadbeef".to_string();
        let intake = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new(sha.clone()).format("pdf".to_string()),
            )
            .expect("register intake");
        let intake_id = intake.intake().intake_id;
        let envelope_path = dir.join(envelope_filename(ItemKind::Paper, intake_id));
        write_envelope(&envelope_path, &extraction, intake_id, &sha).expect("write envelope");
        catalog
            .set_stored_path(ItemKind::Paper, intake_id, &envelope_path.to_string_lossy())
            .expect("set stored path");
        let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Paper);
        attrs.title = extraction.biblio.title.clone();
        attrs.year = extraction.biblio.year.map(|y| y.to_string());
        attrs.doi = extraction.biblio.doi.clone();
        attrs.arxiv_id = extraction.biblio.arxiv_id.clone();
        attrs.container_title = extraction.biblio.container_title.clone();
        attrs.abstract_text = extraction.biblio.abstract_text.clone();
        attrs.language = extraction.biblio.language.clone();
        catalog.upsert_publication_attrs(&attrs).expect("attrs");
        (intake_id, envelope_path)
    }

    #[test]
    fn reaudit_paper_writes_verdict_and_confidence_when_extraction_is_cached() {
        let dir = TempDir::new().unwrap();
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let (intake_id, _) = seed(&mut catalog, dir.path(), Some("10.5555/example"));
        let profile = PaperAuditProfile::default_profile();
        let data = PaperAuditData::default_data();
        let outcome = reaudit_paper(&catalog, intake_id, &profile, &data).expect("reaudit");
        assert_eq!(outcome.intake_id, intake_id);
        assert!(matches!(outcome.verdict.as_str(), "clean" | "needs_work"));
        assert!(matches!(
            outcome.confidence.as_str(),
            "high" | "medium" | "low",
        ));
        // The rollup is now on the row.
        let attrs = catalog
            .publication_attrs(intake_id, ItemKind::Paper)
            .expect("read")
            .expect("row");
        assert_eq!(
            attrs.audit_verdict.as_deref(),
            Some(outcome.verdict.as_str())
        );
        assert_eq!(
            attrs.confidence.as_deref(),
            Some(outcome.confidence.as_str())
        );
    }

    #[test]
    fn override_flips_the_audit_outcome_on_re_run() {
        let dir = TempDir::new().unwrap();
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        // Seed with no DOI so the audit should not floor on
        // identifier (arxiv_id is present).
        let (intake_id, _) = seed(&mut catalog, dir.path(), None);
        let profile = PaperAuditProfile::default_profile();
        let data = PaperAuditData::default_data();
        let first = reaudit_paper(&catalog, intake_id, &profile, &data).expect("first");

        // Void the arxiv_id through an override; now neither DOI nor
        // arxiv is present, so the verdict floors.
        catalog
            .set_override(&NewOverride::new(
                intake_id,
                ItemKind::Paper,
                "arxiv_id",
                None,
                "human",
            ))
            .expect("void");
        let second = reaudit_paper(&catalog, intake_id, &profile, &data).expect("second");
        assert_eq!(
            second.previous_verdict.as_deref(),
            Some(first.verdict.as_str())
        );
        assert_eq!(second.verdict, "needs_work");
    }

    /// Seed the audit projection and the review row the way glean
    /// does at ingest, so the re-audit under test has the state a real
    /// paper carries rather than an empty table.
    fn seed_glean_time_audit(catalog: &Catalog, intake_id: i64) {
        let profile = PaperAuditProfile::default_profile();
        let data = PaperAuditData::default_data();
        let intake = catalog
            .intake_by_id(intake_id)
            .expect("lookup")
            .expect("intake");
        let audited = report_for_intake(catalog, &intake, &profile, &data).expect("first audit");
        let audited_at = catalog.now_iso().expect("now");
        let row = paper_report_to_audit_row(
            &audited.report,
            intake_id,
            ItemKind::Paper.as_scope_str(),
            &profile,
            audited.report.csl_type.map(csl_type_token),
            &audited_at,
            &audited.extractor_version,
            Some("glean_paper-2026-08-04T00:00:00Z-seed"),
        );
        catalog.upsert_node_paper_audit(&row).expect("seed row");
        catalog
            .upsert_review(
                &bookrack_catalog::NewReview::new(
                    intake_id,
                    ItemKind::Paper,
                    "pipeline",
                    bookrack_catalog::STATUS_PENDING,
                )
                .notes(audited.report.to_json()),
            )
            .expect("seed review");
    }

    /// The defect this verb existed to have and did not: the rollup
    /// moved and the projection row — what `papers show` reads — kept
    /// the ingest-time judgement forever.
    #[test]
    fn reaudit_rewrites_the_audit_projection_row() {
        let dir = TempDir::new().unwrap();
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let (intake_id, _) = seed(&mut catalog, dir.path(), None);
        seed_glean_time_audit(&catalog, intake_id);

        let before = catalog
            .node_paper_audit(intake_id, ItemKind::Paper.as_scope_str())
            .expect("read")
            .expect("seeded row");
        assert_eq!(
            before.verdict, "clean",
            "the fixture must start clean or the flip proves nothing",
        );

        // Void the one identifier the paper has; the verdict floors.
        catalog
            .set_override(&NewOverride::new(
                intake_id,
                ItemKind::Paper,
                "arxiv_id",
                None,
                "human",
            ))
            .expect("void");
        let outcome = reaudit_paper(
            &catalog,
            intake_id,
            &PaperAuditProfile::default_profile(),
            &PaperAuditData::default_data(),
        )
        .expect("reaudit");
        assert_eq!(outcome.verdict, "needs_work");

        let after = catalog
            .node_paper_audit(intake_id, ItemKind::Paper.as_scope_str())
            .expect("read")
            .expect("row");
        assert_eq!(
            after.verdict, "needs_work",
            "the projection still reports the ingest-time judgement",
        );
        assert_eq!(
            after.confidence, outcome.confidence,
            "the projection and the rollup must agree after a re-audit",
        );
        let flag_idx = bookrack_catalog::FLAG_COLUMNS
            .iter()
            .position(|c| *c == "flag_no_stable_identifier")
            .expect("column");
        assert_eq!(
            (before.flags[flag_idx], after.flags[flag_idx]),
            (0, 1),
            "the flag columns must move with the judgement, not only the verdict",
        );
    }

    /// Layer 2: the row a per-item re-audit writes belongs to no run.
    /// Reusing the run that first judged the paper would attribute a
    /// later judgement to it; minting an id would name a run the
    /// registry does not hold.
    #[test]
    fn a_reaudited_row_is_attributed_to_no_pipeline_run() {
        let dir = TempDir::new().unwrap();
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let (intake_id, _) = seed(&mut catalog, dir.path(), Some("10.5555/example"));
        seed_glean_time_audit(&catalog, intake_id);
        assert!(
            catalog
                .node_paper_audit(intake_id, ItemKind::Paper.as_scope_str())
                .expect("read")
                .expect("row")
                .pipeline_run_id
                .is_some(),
            "the fixture must start attributed, or the NULL proves nothing",
        );

        reaudit_paper(
            &catalog,
            intake_id,
            &PaperAuditProfile::default_profile(),
            &PaperAuditData::default_data(),
        )
        .expect("reaudit");

        assert_eq!(
            catalog
                .node_paper_audit(intake_id, ItemKind::Paper.as_scope_str())
                .expect("read")
                .expect("row")
                .pipeline_run_id,
            None,
        );
    }

    /// The high-risk half of the write: the report JSON refreshes
    /// while the reviewer's own verdict stays put. An implementation
    /// that reached for `upsert_review` would walk `approved` back to
    /// whatever status it passed.
    #[test]
    fn reaudit_refreshes_the_report_notes_without_touching_the_review_status() {
        let dir = TempDir::new().unwrap();
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let (intake_id, _) = seed(&mut catalog, dir.path(), Some("10.5555/example"));
        seed_glean_time_audit(&catalog, intake_id);

        // A human approves the paper, the way the review verbs do.
        catalog
            .upsert_review(&bookrack_catalog::NewReview::new(
                intake_id,
                ItemKind::Paper,
                "human:curator",
                bookrack_catalog::STATUS_APPROVED,
            ))
            .expect("approve");
        let before = catalog
            .review(intake_id, ItemKind::Paper)
            .expect("read")
            .expect("row");
        assert!(
            before.notes.as_deref().is_some_and(|n| n.contains("clean")),
            "the approve must have preserved the ingest report: {before:?}",
        );

        catalog
            .set_override(&NewOverride::new(
                intake_id,
                ItemKind::Paper,
                "title",
                None,
                "human",
            ))
            .expect("void title");
        reaudit_paper(
            &catalog,
            intake_id,
            &PaperAuditProfile::default_profile(),
            &PaperAuditData::default_data(),
        )
        .expect("reaudit");

        let after = catalog
            .review(intake_id, ItemKind::Paper)
            .expect("read")
            .expect("row");
        assert_eq!(
            after.status,
            bookrack_catalog::STATUS_APPROVED,
            "a re-audit must not reopen a reviewed paper",
        );
        assert_eq!(after.reviewed_by, "human:curator");
        assert_ne!(
            after.notes, before.notes,
            "the stored report must be the one this re-audit computed",
        );
        assert!(
            after
                .notes
                .as_deref()
                .is_some_and(|n| n.contains("needs_work")),
            "the refreshed report must carry the new verdict: {after:?}",
        );
    }

    /// The `grade_*` columns move with the re-audit, which is the
    /// specific claim a curator acts on when correcting a field and
    /// re-running.
    #[test]
    fn reaudit_updates_the_per_field_grades_and_the_judged_csl_type() {
        let dir = TempDir::new().unwrap();
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let (intake_id, _) = seed(&mut catalog, dir.path(), Some("10.5555/example"));
        seed_glean_time_audit(&catalog, intake_id);

        let title_idx = bookrack_catalog::GRADE_COLUMNS
            .iter()
            .position(|(col, _)| *col == "grade_title")
            .expect("grade_title column");
        let before = catalog
            .node_paper_audit(intake_id, ItemKind::Paper.as_scope_str())
            .expect("read")
            .expect("row");
        assert_ne!(before.grades[title_idx], "missing");
        assert_eq!(before.csl_type.as_deref(), Some("paper-conference"));

        catalog
            .set_override(&NewOverride::new(
                intake_id,
                ItemKind::Paper,
                "title",
                None,
                "human",
            ))
            .expect("void title");
        catalog
            .set_override(&NewOverride::new(
                intake_id,
                ItemKind::Paper,
                "csl_type",
                Some("thesis".to_string()),
                "human",
            ))
            .expect("retype");
        reaudit_paper(
            &catalog,
            intake_id,
            &PaperAuditProfile::default_profile(),
            &PaperAuditData::default_data(),
        )
        .expect("reaudit");

        let after = catalog
            .node_paper_audit(intake_id, ItemKind::Paper.as_scope_str())
            .expect("read")
            .expect("row");
        assert_eq!(after.grades[title_idx], "missing");
        assert_eq!(
            after.csl_type.as_deref(),
            Some("thesis"),
            "the column must name the type the judgement actually used",
        );
    }

    #[test]
    fn missing_envelope_yields_missing_envelope_error() {
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("cafebabe".to_string()).format("pdf".to_string()),
            )
            .expect("register");
        // No stored_path set → MissingEnvelope.
        let err = reaudit_paper(
            &catalog,
            intake.intake().intake_id,
            &PaperAuditProfile::default_profile(),
            &PaperAuditData::default_data(),
        )
        .unwrap_err();
        assert!(matches!(err, GleanError::MissingEnvelope(_)));
    }
}
