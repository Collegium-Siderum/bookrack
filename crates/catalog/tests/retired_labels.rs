// SPDX-License-Identifier: Apache-2.0

//! A catalog behind this build that holds rows carrying retired labels
//! is refused by every door that would otherwise read or migrate it.

use std::path::{Path, PathBuf};

use bookrack_catalog::fixtures::{CURRENT_VERSION, create_at_version, user_version};
use bookrack_catalog::{Catalog, CatalogError};
use bookrack_dbkit::RetiredHit;
use rusqlite::Connection;
use tempfile::{TempDir, tempdir};

/// The revision before the one that retires the labels.
const BEFORE: i64 = 17;

const STAMP: &str = "2026-01-01T00:00:00Z";

struct Disk {
    dir: TempDir,
    path: PathBuf,
}

impl Disk {
    /// An empty catalog stopped at `version`.
    fn at(version: i64) -> Disk {
        Disk::named("catalog.db", version)
    }

    fn named(file: &str, version: i64) -> Disk {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(file);
        create_at_version(&path, version);
        Disk { dir, path }
    }

    fn backups(&self) -> PathBuf {
        self.dir.path().join("backups")
    }

    fn sql(&self, sql: &str) {
        Connection::open(&self.path)
            .expect("open fixture")
            .execute_batch(sql)
            .expect("seed fixture");
    }

    /// One intake row with the given `format`, `adapter` and `status`.
    fn intake(&self, sha: &str, format: &str, adapter: &str, status: &str) {
        self.sql(&format!(
            "INSERT INTO intake (source_sha256, intake_at, status, format, adapter) \
             VALUES ('{sha}', '{STAMP}', '{status}', '{format}', '{adapter}');"
        ));
    }

    /// One row of the pipeline-state table `table` that finished the
    /// OCR marker stage.
    fn marker_stage(&self, table: &str) {
        self.sql(&format!(
            "INSERT INTO {table} (book_root_id, intake_id, current_stage, ocr_marker_finished_at) \
             VALUES (1, 1, 'embedded', '{STAMP}');"
        ));
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.path).expect("read fixture")
    }

    fn refusal(&self) -> Vec<RetiredHit> {
        hits(Catalog::open_for_write(&self.path, &self.backups()))
    }
}

fn hits<T>(opened: Result<T, CatalogError>) -> Vec<RetiredHit> {
    match opened {
        Err(CatalogError::RetiredLabels { hits, .. }) => hits,
        Err(other) => panic!("expected a refusal for retired labels, got {other:?}"),
        Ok(_) => panic!("expected a refusal for retired labels, got a catalog"),
    }
}

fn hit(marker: &'static str, count: u64) -> RetiredHit {
    RetiredHit { marker, count }
}

fn migrates(path: &Path) {
    Catalog::open_for_write_unbacked(path).expect("a catalog without retired labels migrates");
    assert_eq!(user_version(path), CURRENT_VERSION);
}

// ── Each label, on a catalog one revision behind ────────────────────

#[test]
fn a_clean_catalog_one_revision_behind_migrates() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-pdf", "pdf", "pdf", "embedded");
    migrates(&disk.path);
}

#[test]
fn the_ocr_format_is_refused() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "ocr-markdown", "pdf", "embedded");
    disk.intake("sha-b", "ocr-markdown", "pdf", "embedded");
    assert_eq!(disk.refusal(), [hit("format = ocr-markdown", 2)]);
}

#[test]
fn the_ocr_adapter_is_refused() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "pdf", "ocr-pages", "embedded");
    assert_eq!(disk.refusal(), [hit("adapter = ocr-pages", 1)]);
}

#[test]
fn the_aborted_status_is_refused() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "pdf", "pdf", "aborted");
    assert_eq!(disk.refusal(), [hit("status = aborted", 1)]);
}

#[test]
fn the_ocr_marker_source_is_refused() {
    let disk = Disk::at(BEFORE);
    disk.sql(
        "INSERT INTO node_publication_attrs (intake_id, scope, source) \
         VALUES (1, 'book', 'ocr_marker');",
    );
    assert_eq!(disk.refusal(), [hit("source = ocr_marker", 1)]);
}

#[test]
fn a_finished_ocr_marker_stage_is_refused() {
    let disk = Disk::at(BEFORE);
    disk.marker_stage("item_state");
    assert_eq!(disk.refusal(), [hit("ocr_marker_finished_at", 1)]);
}

#[test]
fn a_derivation_edge_is_refused() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "pdf", "pdf", "embedded");
    disk.sql("UPDATE intake SET derived_from_sha256 = 'sha-scan';");
    assert_eq!(disk.refusal(), [hit("derived_from_sha256", 1)]);
}

#[test]
fn every_label_a_catalog_holds_is_counted() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "ocr-markdown", "ocr-pages", "embedded");
    disk.intake("sha-b", "ocr-markdown", "ocr-pages", "aborted");
    disk.sql("UPDATE intake SET derived_from_sha256 = 'sha-scan' WHERE source_sha256 = 'sha-a';");
    assert_eq!(
        disk.refusal(),
        [
            hit("format = ocr-markdown", 2),
            hit("adapter = ocr-pages", 2),
            hit("status = aborted", 1),
            hit("derived_from_sha256", 1),
        ]
    );
}

// ── Older revisions: the tables and columns each one has ────────────

#[test]
fn a_clean_catalog_migrates_from_each_side_of_the_two_shape_changes() {
    for version in [7, 8, 13, 14] {
        let disk = Disk::at(version);
        disk.intake("sha-pdf", "pdf", "pdf", "embedded");
        migrates(&disk.path);
    }
}

#[test]
fn the_state_table_is_searched_under_the_name_the_revision_has() {
    for (version, table) in [(7, "book_state"), (8, "item_state")] {
        let disk = Disk::at(version);
        disk.marker_stage(table);
        assert_eq!(
            disk.refusal(),
            [hit("ocr_marker_finished_at", 1)],
            "v{version}"
        );
        assert_eq!(user_version(&disk.path), version);
    }
}

#[test]
fn a_revision_without_the_derivation_column_is_searched_without_it() {
    let disk = Disk::at(13);
    disk.intake("sha-a", "ocr-markdown", "pdf", "embedded");
    assert_eq!(disk.refusal(), [hit("format = ocr-markdown", 1)]);
}

#[test]
fn the_derivation_column_is_searched_from_the_revision_that_adds_it() {
    let disk = Disk::at(14);
    disk.intake("sha-a", "pdf", "pdf", "embedded");
    disk.sql("UPDATE intake SET derived_from_sha256 = 'sha-scan';");
    assert_eq!(disk.refusal(), [hit("derived_from_sha256", 1)]);
}

// ── What a refusal leaves behind ────────────────────────────────────

#[test]
fn a_refused_write_takes_no_backup_however_often_it_is_retried() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "ocr-markdown", "ocr-pages", "embedded");
    for _ in 0..3 {
        disk.refusal();
    }
    assert!(
        !disk.backups().exists(),
        "a refused catalog must not be snapshotted"
    );
    assert_eq!(user_version(&disk.path), BEFORE);
}

#[test]
fn a_clean_catalog_behind_this_build_is_still_backed_up_before_it_migrates() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-pdf", "pdf", "pdf", "embedded");
    Catalog::open_for_write(&disk.path, &disk.backups()).expect("migrate");
    assert_eq!(
        std::fs::read_dir(disk.backups()).expect("backups").count(),
        1
    );
}

#[test]
fn the_refusal_names_the_file_and_counts_each_label() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "ocr-markdown", "ocr-pages", "embedded");
    disk.intake("sha-b", "ocr-markdown", "pdf", "embedded");
    let Err(err) = Catalog::open_for_write(&disk.path, &disk.backups()) else {
        panic!("expected a refusal");
    };
    assert_eq!(
        err.to_string(),
        format!(
            "catalog \"{}\" holds rows from the removed OCR intake path: \
             format = ocr-markdown (2), adapter = ocr-pages (1)",
            disk.path.display()
        )
    );
}

// ── The doors that do not migrate ───────────────────────────────────

#[test]
fn both_read_doors_name_the_labels_rather_than_the_revision() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "ocr-markdown", "ocr-pages", "embedded");
    let before = disk.bytes();
    let expected = [
        hit("format = ocr-markdown", 1),
        hit("adapter = ocr-pages", 1),
    ];
    assert_eq!(hits(Catalog::open(&disk.path)), expected);
    assert_eq!(hits(Catalog::open_read_only(&disk.path)), expected);
    assert_eq!(disk.bytes(), before, "a read leaves the file as it was");
}

#[test]
fn both_read_doors_report_a_clean_catalog_behind_this_build_as_too_old() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-pdf", "pdf", "pdf", "embedded");
    for opened in [
        Catalog::open(&disk.path).map(|_| ()),
        Catalog::open_read_only(&disk.path).map(|_| ()),
    ] {
        assert!(
            matches!(
                opened,
                Err(CatalogError::SchemaTooOld { found: BEFORE, .. })
            ),
            "{opened:?}"
        );
    }
}

#[test]
fn the_bookkeeping_door_declines_a_refused_catalog_like_any_behind_one() {
    let disk = Disk::at(BEFORE);
    disk.intake("sha-a", "ocr-markdown", "ocr-pages", "embedded");
    assert!(
        Catalog::open_for_bookkeeping(&disk.path)
            .expect("declined, not refused")
            .is_none()
    );
}

// ── The papers catalog ──────────────────────────────────────────────

#[test]
fn a_papers_catalog_without_retired_labels_migrates() {
    let disk = Disk::named(bookrack_catalog::PAPERS_CATALOG_FILE, BEFORE);
    disk.intake("sha-paper", "pdf", "pdf", "embedded");
    migrates(&disk.path);
}

// ── A catalog already at this build's revision ──────────────────────

#[test]
fn a_current_catalog_is_not_searched() {
    let disk = Disk::at(CURRENT_VERSION);
    disk.intake("sha-a", "ocr-markdown", "ocr-pages", "aborted");
    Catalog::open_for_write(&disk.path, &disk.backups()).expect("write door");
    Catalog::open_read_only(&disk.path).expect("read door");
}
