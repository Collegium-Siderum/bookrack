// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the source-side provenance the book reads
//! project out of the `intake` row.
//!
//! Every test asserts against the serialized wire shape rather than the
//! DTO struct fields: a field dropped from the projection then fails an
//! assertion instead of breaking the build, and the JSON key names —
//! which are the actual contract — are pinned along with the values.
//! Each pair covers a populated row and a row whose source columns were
//! never written, so an absent column is pinned to `null` rather than to
//! a zero or an empty string.

use std::path::PathBuf;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::ItemKind;
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::dto::BookFilter;
use bookrack_ops::reads::books::{find_books, show_book};
use bookrack_ops::{Caller, Ops};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
    catalog_db: PathBuf,
}

impl Fixture {
    fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let catalog_db = tmp.path().join("catalog.db");
        let corpus_db = tmp.path().join("corpus.db");
        let lancedb_dir = tmp.path().join("lancedb");
        let books_dir = tmp.path().join("books");
        let backup_dir = tmp.path().join("backup");
        Catalog::open(&catalog_db).expect("seed catalog");
        Corpus::open(&corpus_db).expect("seed corpus");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            corpus_db,
            catalog_db.clone(),
            &lancedb_dir,
            books_dir,
            backup_dir,
            Caller::cli(),
        );
        Fixture {
            _tmp: tmp,
            ops,
            catalog_db,
        }
    }

    /// Register one book intake carrying every source-side column the
    /// reads are expected to surface. `page_count` is not a
    /// registration-time column, so it is written separately.
    fn seed_book(&self, sha: &str, original_path: &str, byte_size: i64, page_count: i64) -> i64 {
        let mut catalog = Catalog::open(&self.catalog_db).expect("open catalog");
        let intake_id = catalog
            .register_intake(
                ItemKind::Book,
                &NewIntake::new(sha)
                    .original_path(original_path)
                    .byte_size(byte_size),
            )
            .expect("register intake")
            .into_intake()
            .intake_id;
        catalog
            .set_page_count(ItemKind::Book, intake_id, page_count)
            .expect("set page count");
        intake_id
    }
}

#[test]
fn list_rows_name_the_source_file() {
    let fx = Fixture::build();
    let intake_id = fx.seed_book("sha-summary", "shelf/imported/a-book.pdf", 4096, 612);

    let page = find_books(&fx.ops, BookFilter::default(), 10, 0).expect("find");
    let row = page
        .books
        .iter()
        .find(|b| b.intake_id == intake_id)
        .expect("row");
    let value = serde_json::to_value(row).expect("serialize");

    // The basename, not the recorded path: a list row identifies the
    // file a book came from without carrying a full path per entry.
    assert_eq!(
        value["source_filename"], "a-book.pdf",
        "a list row must name its source file, so a book whose title is \
         missing or a tool placeholder can still be identified"
    );
    assert!(
        value.get("source_path").is_none(),
        "the full path stays in the detail read; list rows carry the basename only"
    );
}

#[test]
fn a_list_row_without_a_recorded_path_carries_a_null_filename() {
    let fx = Fixture::build();
    let mut catalog = Catalog::open(&fx.catalog_db).expect("open catalog");
    let intake_id = catalog
        .register_intake(ItemKind::Book, &NewIntake::new("sha-pathless"))
        .expect("register intake")
        .into_intake()
        .intake_id;
    drop(catalog);

    let page = find_books(&fx.ops, BookFilter::default(), 10, 0).expect("find");
    let row = page
        .books
        .iter()
        .find(|b| b.intake_id == intake_id)
        .expect("row");
    let value = serde_json::to_value(row).expect("serialize");
    assert!(
        value["source_filename"].is_null(),
        "an intake registered without an original path has no filename to report"
    );
}

#[test]
fn the_detail_read_carries_the_page_count_and_byte_size_of_its_intake_row() {
    let fx = Fixture::build();
    let intake_id = fx.seed_book("sha-detail", "shelf/imported/another-book.pdf", 8192, 240);

    let detail = show_book(&fx.ops, intake_id).expect("show");
    let value = serde_json::to_value(&detail).expect("serialize");

    assert_eq!(
        value["page_count"], 240,
        "the detail read reports the sheet count recorded on the intake row"
    );
    assert_eq!(
        value["byte_size"], 8192,
        "the detail read reports the source size recorded at registration"
    );
    // The four fields the same read already surfaced stay put alongside
    // the two new ones.
    assert_eq!(value["source_path"], "shelf/imported/another-book.pdf");
    assert_eq!(value["source_filename"], "another-book.pdf");
}

#[test]
fn the_detail_read_reports_absent_source_columns_as_null() {
    let fx = Fixture::build();
    let mut catalog = Catalog::open(&fx.catalog_db).expect("open catalog");
    let intake_id = catalog
        .register_intake(ItemKind::Book, &NewIntake::new("sha-bare"))
        .expect("register intake")
        .into_intake()
        .intake_id;
    drop(catalog);

    let detail = show_book(&fx.ops, intake_id).expect("show");
    let value = serde_json::to_value(&detail).expect("serialize");

    // Reflow formats never get a page count, and a row may predate the
    // byte-size column; neither is reported as a zero.
    assert!(
        value["page_count"].is_null(),
        "an unpaginated source has no sheet count, not a count of zero"
    );
    assert!(
        value["byte_size"].is_null(),
        "an unrecorded size is absent, not a size of zero"
    );
}
