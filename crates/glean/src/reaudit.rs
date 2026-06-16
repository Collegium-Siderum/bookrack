// SPDX-License-Identifier: Apache-2.0

//! Offline paper-side metadata re-audit: re-run the audit against the
//! current effective metadata, using the extraction cached in the
//! paper's intake-store envelope.
//!
//! No source file is re-extracted and nothing bibliographic is
//! written: the base attrs, the contributors, and the review status
//! all stay as they are. The only writes are the `confidence` /
//! `audit_verdict` rollup on `node_publication_attrs` and one
//! pipeline-audit row — so after correcting fields through the
//! metadata write surface, the stored verdict catches up with the
//! corrections instead of reporting the glean-time outcome forever.
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

use crate::audit::{
    PaperAuditData, PaperAuditInput, PaperAuditProfile, PaperReport, PaperVerdict, audit_paper,
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
/// cached extraction, writing back only the `confidence` /
/// `audit_verdict` rollup.
///
/// The audit grades the *effective* metadata — base attrs merged
/// with any overrides — against the same extraction-time signals
/// the glean audit used, read back from the envelope. Returns
/// [`GleanError::MissingEnvelope`] when the intake has no readable
/// envelope and [`GleanError::EnvelopeMismatch`] when the envelope
/// belongs to a different source file.
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

    let report = report_for_intake(catalog, &intake, profile, data)?;

    let confidence = report.confidence.as_token().to_string();
    let verdict = report.verdict.as_token().to_string();
    catalog.update_audit_rollup(intake_id, ItemKind::Paper, &confidence, &verdict)?;

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
    report_for_intake(catalog, &intake, profile, data)
}

/// Audit one intake's effective metadata against its cached
/// extraction envelope: read and verify the envelope, assemble the
/// [`PaperAuditInput`], and run the audit.
fn report_for_intake(
    catalog: &Catalog,
    intake: &Intake,
    profile: &PaperAuditProfile,
    data: &PaperAuditData,
) -> Result<PaperReport> {
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
    Ok(audit_paper(&input, profile, data))
}

/// One run id ties every audit row from a maintenance operation
/// together. The `glean-{op}-` prefix distinguishes paper-side
/// reaudit rows from ingest's `ingest-{op}-` prefix when a mixed log
/// is inspected.
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
                title: Some("Attention Is All You Need".to_string()),
                subtitle: None,
                publisher: None,
                year: Some(2017),
                year_raw: Some("2017".to_string()),
                isbn: None,
                series: None,
                language: Some("en".to_string()),
                contributors: vec![Contributor {
                    name: "Ashish Vaswani".to_string(),
                    role: ContributorRole::Author,
                    family: Some("Vaswani".to_string()),
                    given: Some("Ashish".to_string()),
                    orcid: None,
                }],
                doi: doi.map(|s| s.to_string()),
                arxiv_id: Some("1706.03762".to_string()),
                issn: None,
                container_title: Some("NeurIPS Proceedings".to_string()),
                abstract_text: Some(
                    "The dominant sequence transduction models are based on complex \
                     recurrent or convolutional neural networks that include an encoder \
                     and a decoder. The best performing models also connect the encoder \
                     and decoder through an attention mechanism. We propose a new simple \
                     network architecture, the Transformer, based solely on attention \
                     mechanisms, dispensing with recurrence and convolutions entirely."
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
