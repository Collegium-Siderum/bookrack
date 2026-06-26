// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the paginated catalog-browse reads.
//!
//! Each test seeds a tempdir-backed catalog and drives one paginated
//! read through the `ops` surface. The focus is on the `truncated`
//! flag and on the page assembly through the batched catalog
//! accessors.

use std::path::PathBuf;

use bookrack_catalog::{Catalog, NewIntake, NewPublicationAttrs};
use bookrack_core::ItemKind;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::dto::{BookFilter, MAX_LIST_LIMIT};
use bookrack_ops::reads::books::find_books;
use bookrack_ops::reads::metadata::list_metadata;
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

    fn catalog(&self) -> Catalog {
        Catalog::open(&self.catalog_db).expect("open catalog")
    }

    fn seed_book(&self, sha: &str, title: &str) -> i64 {
        let mut catalog = self.catalog();
        let intake_id = catalog
            .register_intake(ItemKind::Book, &NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id;
        let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Book);
        attrs.title = Some(title.into());
        catalog
            .upsert_publication_attrs(&attrs)
            .expect("seed attrs");
        intake_id
    }
}

#[test]
fn find_books_reports_not_truncated_when_the_page_holds_every_match() {
    let fx = Fixture::build();
    let _ = fx.seed_book("sha-a", "Alpha");
    let _ = fx.seed_book("sha-b", "Bravo");

    // Caller asks for more than MAX_LIST_LIMIT so the clamp engages;
    // total still fits in one clamped page, so `truncated` must be
    // false.
    let request_over_max = MAX_LIST_LIMIT + 50;
    let page = find_books(&fx.ops, BookFilter::default(), request_over_max, 0).expect("find");
    assert_eq!(page.total, 2);
    assert_eq!(page.books.len(), 2);
    assert!(
        !page.truncated,
        "page covers every matching row; truncated must be false"
    );
}

#[test]
fn find_books_reports_truncated_when_more_rows_remain() {
    let fx = Fixture::build();
    let _ = fx.seed_book("sha-a", "Alpha");
    let _ = fx.seed_book("sha-b", "Bravo");
    let _ = fx.seed_book("sha-c", "Charlie");

    let page = find_books(&fx.ops, BookFilter::default(), 2, 0).expect("find");
    assert_eq!(page.total, 3);
    assert_eq!(page.books.len(), 2);
    assert!(page.truncated, "one row remains past this page");
}

#[test]
fn find_books_batched_enrichment_resolves_title_per_intake() {
    // The batched effective-attrs accessor must thread each title back
    // to its own intake. Spread several rows across one page to catch
    // a misordered zip.
    let fx = Fixture::build();
    let ids: Vec<i64> = (0..5)
        .map(|n| fx.seed_book(&format!("sha-{n}"), &format!("Title {n}")))
        .collect();

    let page = find_books(&fx.ops, BookFilter::default(), 10, 0).expect("find");
    assert_eq!(page.total, 5);
    assert_eq!(page.books.len(), 5);
    for (n, intake_id) in ids.iter().enumerate() {
        let expected = format!("Title {n}");
        let row = page
            .books
            .iter()
            .find(|b| b.intake_id == *intake_id)
            .expect("row");
        assert_eq!(row.title.as_deref(), Some(expected.as_str()));
    }
}

#[test]
fn list_metadata_reports_not_truncated_when_the_clamped_page_covers_everything() {
    let fx = Fixture::build();
    let _ = fx.seed_book("sha-a", "Alpha");
    let _ = fx.seed_book("sha-b", "Bravo");

    let request_over_max = MAX_LIST_LIMIT + 1;
    let page = list_metadata(&fx.ops, request_over_max, 0).expect("list");
    assert_eq!(page.total, 2);
    assert_eq!(page.rows.len(), 2);
    assert!(!page.truncated);
}
