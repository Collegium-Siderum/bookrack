// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the recomputed metadata report read.
//!
//! Each test builds a catalog-only [`Ops`] over a tempdir-backed
//! catalog, seeds an intake with a cached extraction envelope, and
//! checks that `show_metadata_report` grades the effective view
//! without writing anything back.

use std::path::PathBuf;

use bookrack_catalog::{Catalog, NewIntake, NewOverride, NewPublicationAttrs};
use bookrack_core::ItemKind;
use bookrack_embed::OllamaEmbedClient;
use bookrack_extract::{Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc};
use bookrack_extract::{envelope_filename, write_envelope};
use bookrack_ops::reads::metadata::show_metadata_report;
use bookrack_ops::{AuditData, AuditProfile, Caller, Ops, OpsError};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
    catalog_db: PathBuf,
    books_dir: PathBuf,
}

impl Fixture {
    fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let catalog_db = tmp.path().join("catalog.db");
        let corpus_db = tmp.path().join("corpus.db");
        let lancedb_dir = tmp.path().join("lancedb");
        let books_dir = tmp.path().join("books");
        let backup_dir = tmp.path().join("backup");
        std::fs::create_dir_all(&books_dir).expect("books dir");
        // Create the catalog once so the schema is migrated before the
        // first op runs.
        Catalog::open(&catalog_db).expect("seed catalog");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            corpus_db,
            catalog_db.clone(),
            &lancedb_dir,
            books_dir.clone(),
            backup_dir,
            Caller::cli(),
        );
        Fixture {
            _tmp: tmp,
            ops,
            catalog_db,
            books_dir,
        }
    }
}

fn sample_extraction() -> Extraction {
    Extraction {
        blocks: vec![
            Block {
                kind: BlockKind::Heading { level: 1 },
                text: "Chapter One".into(),
                source_unit: 0,
                style: None,
            },
            Block {
                kind: BlockKind::Body,
                text: "Some sample prose for the audit body sample.".into(),
                source_unit: 0,
                style: None,
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

fn seed_book(fx: &Fixture, sha: &str) -> i64 {
    let mut catalog = Catalog::open(&fx.catalog_db).expect("open catalog");
    let intake_id = catalog
        .register_intake(
            ItemKind::Book,
            &NewIntake::new(sha.to_string()).format("txt").byte_size(1),
        )
        .expect("register")
        .intake()
        .intake_id;
    let path = fx
        .books_dir
        .join(envelope_filename(ItemKind::Book, intake_id));
    write_envelope(&path, &sample_extraction(), intake_id, sha).expect("write envelope");
    catalog
        .set_stored_path(ItemKind::Book, intake_id, &path.to_string_lossy())
        .expect("stored path");
    intake_id
}

#[test]
fn report_grades_the_effective_view_and_writes_nothing() {
    let fx = Fixture::build();
    let id = seed_book(&fx, "sha-report-read");

    let catalog = Catalog::open(&fx.catalog_db).expect("open catalog");
    // Seed a stale stored rollup the report must surface untouched.
    let mut attrs = NewPublicationAttrs::new(id, ItemKind::Book);
    attrs.title = Some("A Plain Title".to_string());
    attrs.confidence = Some("low".to_string());
    attrs.audit_verdict = Some("needs_work".to_string());
    catalog.upsert_publication_attrs(&attrs).expect("attrs");
    // A curated field must reach the report through the effective view.
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

    let report = show_metadata_report(
        &fx.ops,
        id,
        &AuditData::default_data(),
        &AuditProfile::default(),
    )
    .expect("report");

    assert_eq!(report.intake_id, id);
    assert_eq!(report.profile, AuditProfile::default().name);
    let publisher = report
        .fields
        .iter()
        .find(|f| f.field == "publisher")
        .expect("publisher row");
    assert_ne!(publisher.grade, "missing");
    assert_eq!(publisher.origin, "override");
    let year = report
        .fields
        .iter()
        .find(|f| f.field == "year")
        .expect("year row");
    assert_eq!(year.origin, "voided");
    assert_eq!(year.grade, "medium");
    assert!(year.flags.iter().any(|f| f == "voided"));
    let title = report
        .fields
        .iter()
        .find(|f| f.field == "title")
        .expect("title row");
    assert_eq!(title.origin, "extracted");
    assert!(report.fields.iter().all(|f| !f.hint.is_empty()));
    assert!(matches!(report.verdict.as_str(), "clean" | "needs_work"));
    assert!(matches!(
        report.confidence.as_str(),
        "low" | "medium" | "high"
    ));

    // The stored rollup rides along for comparison...
    assert_eq!(report.stored_verdict.as_deref(), Some("needs_work"));
    assert_eq!(report.stored_confidence.as_deref(), Some("low"));
    assert_eq!(report.review_status, None);

    // ...and stays exactly as seeded: the read writes nothing back.
    let stored = catalog
        .publication_attrs(id, ItemKind::Book)
        .expect("read attrs")
        .expect("attrs row");
    assert_eq!(stored.audit_verdict.as_deref(), Some("needs_work"));
    assert_eq!(stored.confidence.as_deref(), Some("low"));
}

#[test]
fn unknown_book_maps_to_intake_not_found() {
    let fx = Fixture::build();
    let err = show_metadata_report(
        &fx.ops,
        9999,
        &AuditData::default_data(),
        &AuditProfile::default(),
    )
    .expect_err("no intake");
    assert!(matches!(err, OpsError::IntakeNotFound { intake_id: 9999 }));
}

#[test]
fn missing_envelope_is_an_error() {
    let fx = Fixture::build();
    let mut catalog = Catalog::open(&fx.catalog_db).expect("open catalog");
    let id = catalog
        .register_intake(
            ItemKind::Book,
            &NewIntake::new("sha-no-envelope".to_string())
                .format("txt")
                .byte_size(1),
        )
        .expect("register")
        .intake()
        .intake_id;
    drop(catalog);

    let err = show_metadata_report(
        &fx.ops,
        id,
        &AuditData::default_data(),
        &AuditProfile::default(),
    )
    .expect_err("no envelope");
    assert!(!matches!(err, OpsError::IntakeNotFound { .. }));
}
