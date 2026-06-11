// SPDX-License-Identifier: Apache-2.0

//! Offline metadata re-audit: re-run the plausibility audit against the
//! current effective metadata, using the extraction cached in the book's
//! intake-store envelope.
//!
//! No source file is re-extracted and nothing bibliographic is written:
//! the base attrs, the contributors, the overrides, and the review
//! status all stay as they are. The only writes are the `confidence` /
//! `audit_verdict` rollup on `node_publication_attrs` and one
//! pipeline-audit row — so after correcting fields through the metadata
//! write surface, the stored verdict can catch up with the corrections
//! instead of reporting the ingest-time outcome forever.
//!
//! [`build_report`] exposes the same computation with no write at all:
//! it returns the full per-field report for read surfaces that need the
//! grades, flags, and hints rather than the two-scalar rollup.

use std::path::Path;
use std::time::Instant;

use bookrack_catalog::{Catalog, Intake};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_metadata::{AuditData, AuditProfile};

use crate::{
    IngestError, Result, audit_as, audit_metric_summary, body_sample, maintenance_run_id, structure,
};
use bookrack_extract::envelope::{self, EnvelopeError};

/// What one [`reaudit_book`] call computed and stored.
#[derive(Debug, Clone)]
pub struct ReauditOutcome {
    /// The book that was re-audited.
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

/// Re-run the metadata audit for one book from its cached extraction,
/// writing back only the `confidence` / `audit_verdict` rollup.
///
/// The audit grades the *effective* metadata — base attrs merged with
/// the curator's overrides — against the same extraction-time signals
/// the ingest audit used, read back from the envelope. Returns
/// [`IngestError::MissingEnvelope`] when the intake has no readable
/// envelope and [`IngestError::EnvelopeMismatch`] when the envelope
/// belongs to a different source file.
pub fn reaudit_book(
    catalog: &Catalog,
    intake_id: i64,
    audit_data: &AuditData,
    audit_profile: &AuditProfile,
) -> Result<ReauditOutcome> {
    let intake = catalog
        .intake_by_id(intake_id)?
        .ok_or(IngestError::UnknownIntake(intake_id))?;

    let book_root_id = PartitionIdx::new(intake_id).root().get();
    let started = Instant::now();
    let run_id = maintenance_run_id("reaudit");

    let previous = catalog.publication_attrs(intake_id, ItemKind::Book)?;
    let previous_verdict = previous.as_ref().and_then(|a| a.audit_verdict.clone());
    let previous_confidence = previous.as_ref().and_then(|a| a.confidence.clone());

    let report = report_for_intake(catalog, &intake, audit_data, audit_profile)?;

    let confidence = report.confidence.as_str().to_string();
    let verdict = report.verdict.as_token().to_string();
    catalog.update_audit_rollup(intake_id, ItemKind::Book, &confidence, &verdict)?;

    let outcome = match report.verdict {
        bookrack_metadata::Verdict::Clean => "ok",
        bookrack_metadata::Verdict::NeedsWork => "partial",
    };
    audit_as(
        catalog,
        "reaudit",
        &run_id,
        &intake.source_sha256,
        Some(book_root_id),
        "metadata",
        "audit",
        outcome,
        started,
        Some(audit_metric_summary(&report)),
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

/// Rebuild the metadata audit report for one book from its cached
/// extraction, with no write-back: the rollup, the review row, and the
/// pipeline trail all stay as they are.
///
/// Grades the same *effective* metadata view as [`reaudit_book`] and
/// returns the full [`bookrack_metadata::MetadataReport`] — per-field
/// grades, flags, and hints plus the shape flags — instead of the
/// two-scalar rollup. Returns [`IngestError::MissingEnvelope`] when
/// the intake has no readable envelope and
/// [`IngestError::EnvelopeMismatch`] when the envelope belongs to a
/// different source file.
pub fn build_report(
    catalog: &Catalog,
    intake_id: i64,
    audit_data: &AuditData,
    audit_profile: &AuditProfile,
) -> Result<bookrack_metadata::MetadataReport> {
    let intake = catalog
        .intake_by_id(intake_id)?
        .ok_or(IngestError::UnknownIntake(intake_id))?;
    report_for_intake(catalog, &intake, audit_data, audit_profile)
}

/// Audit one intake's effective metadata against its cached extraction
/// envelope: read and verify the envelope, assemble the
/// [`bookrack_metadata::AuditInput`], and run the audit.
fn report_for_intake(
    catalog: &Catalog,
    intake: &Intake,
    audit_data: &AuditData,
    audit_profile: &AuditProfile,
) -> Result<bookrack_metadata::MetadataReport> {
    let intake_id = intake.intake_id;
    let stored_path = intake
        .stored_path
        .as_deref()
        .ok_or(IngestError::MissingEnvelope(intake_id))?;
    let envelope = match envelope::read_envelope(Path::new(stored_path)) {
        Ok(env) => env,
        Err(EnvelopeError::Io(_)) => return Err(IngestError::MissingEnvelope(intake_id)),
        Err(e) => return Err(e.into()),
    };
    if envelope.source_sha256 != intake.source_sha256 {
        return Err(IngestError::EnvelopeMismatch(intake_id));
    }

    let effective = catalog.effective_publication_attrs(intake_id, ItemKind::Book)?;
    let origins = crate::field_origins(catalog, intake_id)?;
    let toc_stats = structure::toc_stats(&envelope.extraction, &audit_profile.toc_shape);
    let sample = body_sample(&envelope.extraction);
    let source_stem = intake
        .original_path
        .as_deref()
        .and_then(|p| Path::new(p).file_stem())
        .map(|s| s.to_string_lossy().into_owned());
    let input = bookrack_metadata::AuditInput {
        biblio: &envelope.extraction.biblio,
        provenance: &envelope.extraction.provenance,
        effective: &effective,
        toc_stats: &toc_stats,
        body_sample: &sample,
        total_blocks: envelope.extraction.blocks.len(),
        source_stem: source_stem.as_deref(),
        data: audit_data,
        origins,
    };
    Ok(bookrack_metadata::audit(&input, audit_profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    use bookrack_catalog::{NewIntake, NewOverride, NewPublicationAttrs};
    use bookrack_extract::{
        Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc,
    };

    use bookrack_extract::envelope::{envelope_filename, write_envelope};

    fn sample_extraction() -> Extraction {
        Extraction {
            blocks: vec![
                Block {
                    kind: BlockKind::Heading { level: 1 },
                    text: "Chapter One".into(),
                    source_unit: 0,
                },
                Block {
                    kind: BlockKind::Body,
                    text: "Some sample prose for the audit body sample.".into(),
                    source_unit: 0,
                },
            ],
            toc: Toc::default(),
            biblio: Biblio::default(),
            provenance: Provenance {
                adapter: "txt".into(),
                extractor_version: 1,
                text_layer_quality: TextLayerQuality::BornDigital,
                skipped_units: vec![],
                derived_from_sha256: None,
                partial_pages: None,
            },
        }
    }

    fn seed_book(catalog: &mut Catalog, books_dir: &Path, sha: &str) -> i64 {
        let intake_id = catalog
            .register_intake(&NewIntake::new(sha.to_string()).format("txt").byte_size(1))
            .expect("register")
            .intake()
            .intake_id;
        let path = books_dir.join(envelope_filename(intake_id));
        write_envelope(&path, &sample_extraction(), intake_id, sha).expect("write envelope");
        catalog
            .set_stored_path(intake_id, &path.to_string_lossy())
            .expect("stored path");
        intake_id
    }

    #[test]
    fn reaudit_refreshes_the_rollup_and_appends_a_trail_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let id = seed_book(&mut catalog, dir.path(), "sha-reaudit");

        // Seed a stale ingest-time rollup the re-audit must replace.
        let mut attrs = NewPublicationAttrs::new(id, ItemKind::Book);
        attrs.title = Some("A Plain Title".to_string());
        attrs.confidence = Some("low".to_string());
        attrs.audit_verdict = Some("needs_work".to_string());
        catalog.upsert_publication_attrs(&attrs).expect("attrs");
        // A curated field must reach the audit through the effective view.
        catalog
            .set_override(&NewOverride::new(
                id,
                ItemKind::Book,
                "publisher",
                Some("A Curated Publisher".to_string()),
                "human",
            ))
            .expect("override");

        let outcome = reaudit_book(
            &catalog,
            id,
            &AuditData::default_data(),
            &AuditProfile::default(),
        )
        .expect("reaudit");
        assert_eq!(outcome.previous_verdict.as_deref(), Some("needs_work"));
        assert_eq!(outcome.previous_confidence.as_deref(), Some("low"));

        // The stored rollup now matches what the re-audit computed, and
        // the bibliographic columns survived the targeted update.
        let stored = catalog
            .publication_attrs(id, ItemKind::Book)
            .expect("read attrs")
            .expect("attrs row");
        assert_eq!(
            stored.audit_verdict.as_deref(),
            Some(outcome.verdict.as_str())
        );
        assert_eq!(
            stored.confidence.as_deref(),
            Some(outcome.confidence.as_str())
        );
        assert_eq!(stored.title.as_deref(), Some("A Plain Title"));

        // One trail row from the reaudit entry point.
        let rows = catalog
            .pipeline_audit_for_book(PartitionIdx::new(id).root().get())
            .expect("trail");
        let last = rows.last().expect("trail rows");
        assert_eq!(last.stage, "metadata");
        assert_eq!(last.actor_detail.as_deref(), Some("reaudit"));
        assert!(last.pipeline_run_id.starts_with("reaudit-"));
    }

    #[test]
    fn reaudit_requires_a_readable_envelope() {
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let id = catalog
            .register_intake(&NewIntake::new("sha-no-envelope").format("txt").byte_size(1))
            .expect("register")
            .intake()
            .intake_id;
        let err = reaudit_book(
            &catalog,
            id,
            &AuditData::default_data(),
            &AuditProfile::default(),
        )
        .expect_err("no envelope");
        assert!(matches!(err, IngestError::MissingEnvelope(i) if i == id));

        let _ = &mut catalog;
        let err = reaudit_book(
            &catalog,
            9999,
            &AuditData::default_data(),
            &AuditProfile::default(),
        )
        .expect_err("no intake");
        assert!(matches!(err, IngestError::UnknownIntake(9999)));
    }

    #[test]
    fn build_report_returns_field_rows_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let id = seed_book(&mut catalog, dir.path(), "sha-report");

        let mut attrs = NewPublicationAttrs::new(id, ItemKind::Book);
        attrs.title = Some("A Plain Title".to_string());
        catalog.upsert_publication_attrs(&attrs).expect("attrs");
        catalog
            .set_override(&NewOverride::new(
                id,
                ItemKind::Book,
                "publisher",
                Some("A Curated Publisher".to_string()),
                "human",
            ))
            .expect("override");

        let report = build_report(
            &catalog,
            id,
            &AuditData::default_data(),
            &AuditProfile::default(),
        )
        .expect("report");

        // The report grades the effective view, so the curated
        // publisher appears as a graded row; every row carries a hint.
        let publisher = report
            .fields
            .iter()
            .find(|f| f.field == "publisher")
            .expect("publisher row");
        assert!(!matches!(
            publisher.grade,
            bookrack_metadata::FieldGrade::Missing
        ));
        assert!(report.fields.iter().all(|f| !f.hint.is_empty()));

        // No write-back: the rollup stays unset and the trail stays
        // empty.
        let stored = catalog
            .publication_attrs(id, ItemKind::Book)
            .expect("read attrs")
            .expect("attrs row");
        assert_eq!(stored.audit_verdict, None);
        assert_eq!(stored.confidence, None);
        let rows = catalog
            .pipeline_audit_for_book(PartitionIdx::new(id).root().get())
            .expect("trail");
        assert!(rows.is_empty());

        let err = build_report(
            &catalog,
            9999,
            &AuditData::default_data(),
            &AuditProfile::default(),
        )
        .expect_err("no intake");
        assert!(matches!(err, IngestError::UnknownIntake(9999)));
    }

    #[test]
    fn build_report_reads_override_origins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let id = seed_book(&mut catalog, dir.path(), "sha-origins");

        let mut attrs = NewPublicationAttrs::new(id, ItemKind::Book);
        attrs.title = Some("A Plain Title".to_string());
        attrs.year = Some("2005".to_string());
        catalog.upsert_publication_attrs(&attrs).expect("attrs");
        catalog
            .set_override(&NewOverride::new(
                id,
                ItemKind::Book,
                "publisher",
                Some("A Curated Publisher".to_string()),
                "human",
            ))
            .expect("override");
        catalog
            .set_override(&NewOverride::new(id, ItemKind::Book, "year", None, "human"))
            .expect("void");

        let report = build_report(
            &catalog,
            id,
            &AuditData::default_data(),
            &AuditProfile::default(),
        )
        .expect("report");

        // The fixture extraction is txt, the weakest prior: extracted
        // fields are downgraded, the curated publisher is not, and the
        // voided year reads as a deliberate gap.
        let by_name = |name: &str| {
            report
                .fields
                .iter()
                .find(|f| f.field == name)
                .unwrap_or_else(|| panic!("{name} row"))
        };
        let title = by_name("title");
        assert!(
            title
                .flags
                .contains(&bookrack_metadata::Flag::SourcePriorWeak)
        );
        let publisher = by_name("publisher");
        assert!(
            !publisher
                .flags
                .contains(&bookrack_metadata::Flag::SourcePriorWeak)
        );
        assert_eq!(publisher.grade, bookrack_metadata::FieldGrade::Strong);
        let year = by_name("year");
        assert_eq!(year.grade, bookrack_metadata::FieldGrade::Medium);
        assert_eq!(year.flags, vec![bookrack_metadata::Flag::Voided]);
    }
}
