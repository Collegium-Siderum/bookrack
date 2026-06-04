// SPDX-License-Identifier: Apache-2.0

//! Anchors the intake format commitment: a `catalog.db` written by an
//! earlier `bookrack-catalog` binary opens cleanly under the current
//! one and every intake row round-trips, with each field's value
//! surviving any schema rebuild in between.
//!
//! The committed fixture at `tests/fixtures/intake/v1/catalog.db` is
//! the executable form of the commitment. Regenerate it with
//! `cargo test --package bookrack-catalog --test intake_format -- \
//!  --ignored regenerate_intake_v1_fixture` after a deliberate
//! commitment change; otherwise leave it alone so the round-trip
//! test continues to assert backward compatibility.

use std::path::{Path, PathBuf};

use bookrack_catalog::{Catalog, IntakeStatus, NewIntake};
use tempfile::tempdir;

/// Value of `bookrack_extract::EXTRACTOR_VERSION` at the moment this
/// fixture was generated. Hardcoded rather than imported because
/// `bookrack-catalog` sits upstream of `bookrack-extract` in the
/// dependency graph; the commitment is that the int stored in the
/// fixture survives unchanged.
const FIXTURE_EXTRACTOR_VERSION: u32 = 1;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/intake/v1")
}

fn fixture_db() -> PathBuf {
    fixture_root().join("catalog.db")
}

/// A row of fixture data, defined once so the generator and the
/// round-trip reader read off the same expected values.
struct Row {
    sha: &'static str,
    original_path: &'static str,
    format: &'static str,
    byte_size: i64,
    adapter: Option<&'static str>,
    final_status: IntakeStatus,
}

const ROWS: &[Row] = &[
    Row {
        sha: "rt-row-1-sha",
        original_path: "incoming/alpha.epub",
        format: "epub",
        byte_size: 8192,
        adapter: Some("epub"),
        final_status: IntakeStatus::Embedded,
    },
    Row {
        sha: "rt-row-2-sha",
        original_path: "incoming/beta.pdf",
        format: "pdf",
        byte_size: 131072,
        adapter: Some("pdf"),
        final_status: IntakeStatus::Extracted,
    },
    Row {
        sha: "rt-row-3-sha",
        original_path: "incoming/gamma.txt",
        format: "text",
        byte_size: 2048,
        adapter: None,
        final_status: IntakeStatus::Pending,
    },
];

#[test]
fn intake_v1_fixture_round_trips() {
    let fixture_src = fixture_db();
    assert!(
        fixture_src.exists(),
        "missing fixture at {}; regenerate via the ignored generator test",
        fixture_src.display()
    );

    // Open through a temp copy so a CI run never mutates the committed
    // fixture file even though `Catalog::open` migrates and verifies.
    let dir = tempdir().expect("tempdir");
    let target = dir.path().join("catalog.db");
    std::fs::copy(&fixture_src, &target).expect("copy fixture");

    let catalog = Catalog::open(&target).expect("open migrated fixture");

    assert_eq!(catalog.count_intakes().expect("count"), ROWS.len() as u64);

    for (index, expected) in ROWS.iter().enumerate() {
        let intake_id = (index as i64) + 1;
        let row = catalog
            .intake_by_id(intake_id)
            .expect("lookup")
            .unwrap_or_else(|| panic!("expected row at intake_id={intake_id}"));
        assert_eq!(row.intake_id, intake_id);
        assert_eq!(row.source_sha256, expected.sha);
        assert_eq!(row.original_path.as_deref(), Some(expected.original_path));
        assert_eq!(row.format.as_deref(), Some(expected.format));
        assert_eq!(row.byte_size, Some(expected.byte_size));
        assert_eq!(row.adapter.as_deref(), expected.adapter);
        assert_eq!(row.extractor_version, FIXTURE_EXTRACTOR_VERSION);
        assert_eq!(row.status, expected.final_status);
        assert!(
            !row.intake_at.is_empty(),
            "intake_at must survive the rebuild"
        );
    }

    // sqlite_sequence survives the M[4] rebuild, so the next
    // registration receives an id past the highest fixture row.
    let mut next_catalog = catalog;
    let fresh = next_catalog
        .register_intake(&NewIntake::new("rt-fresh-sha"))
        .expect("register fresh");
    assert!(
        fresh.intake().intake_id > ROWS.len() as i64,
        "next intake_id ({}) must exceed the fixture max ({})",
        fresh.intake().intake_id,
        ROWS.len()
    );
}

/// Rebuilds the v1 fixture using the current `bookrack-catalog` write
/// API. Ignored by default so CI never overwrites the committed file;
/// run on purpose when the commitment itself changes.
#[test]
#[ignore]
fn regenerate_intake_v1_fixture() {
    let dst_dir = fixture_root();
    std::fs::create_dir_all(&dst_dir).expect("create fixture dir");
    let dst = dst_dir.join("catalog.db");
    if dst.exists() {
        std::fs::remove_file(&dst).expect("clear previous fixture");
    }

    let mut catalog = Catalog::open(&dst).expect("open new catalog");
    for row in ROWS {
        let id = catalog
            .register_intake(
                &NewIntake::new(row.sha)
                    .original_path(row.original_path)
                    .format(row.format)
                    .byte_size(row.byte_size),
            )
            .expect("register")
            .into_intake()
            .intake_id;
        if let Some(adapter) = row.adapter {
            catalog
                .set_extraction(id, adapter, FIXTURE_EXTRACTOR_VERSION)
                .expect("set_extraction");
        }
        if row.final_status != IntakeStatus::Pending {
            catalog
                .set_intake_status(id, row.final_status)
                .expect("set_intake_status");
        }
    }
}
