// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the book-side reads on a library whose
//! `catalog.db` has not been created yet.
//!
//! The book catalog is materialised by `init` or the first ingest, so
//! a root that was only registered has none. The reads must treat that
//! as "no books" — an empty page, zero counts, an unknown intake —
//! rather than as an open failure, and must not create the file as a
//! side effect of looking.

use std::path::{Path, PathBuf};

use bookrack_catalog::IntakeStatus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::dto::{BookFilter, MetadataFilter, ShowTocArgs};
use bookrack_ops::reads::books::{
    category_counts, find_books, list_books, list_ocr_pending, show_book, show_stats, show_toc,
};
use bookrack_ops::reads::metadata::{
    list_metadata, list_pending_reviews, show_audit_trail, show_metadata_audit,
    show_metadata_report,
};
use bookrack_ops::reads::pipeline::show_pipeline_trail;
use bookrack_ops::{AuditData, AuditProfile, Caller, Ops, OpsError};
use tempfile::TempDir;

struct Fixture {
    tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
    catalog_db: PathBuf,
}

impl Fixture {
    /// An `Ops` over a data root that holds no store file at all.
    fn empty_root() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let catalog_db = tmp.path().join("catalog.db");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            tmp.path().join("corpus.db"),
            catalog_db.clone(),
            &tmp.path().join("lancedb"),
            tmp.path().join("books"),
            tmp.path().join("backup"),
            Caller::cli(),
        );
        assert!(
            !catalog_db.exists(),
            "fixture precondition: no book catalog on disk"
        );
        Fixture {
            tmp,
            ops,
            catalog_db,
        }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn assert_nothing_created(&self, what: &str) {
        assert!(
            !self.catalog_db.exists(),
            "{what} must not create the book catalog"
        );
        let entries: Vec<PathBuf> = std::fs::read_dir(self.root())
            .expect("read data root")
            .map(|e| e.expect("dir entry").path())
            .collect();
        assert!(entries.is_empty(), "{what} left files behind: {entries:?}");
    }
}

fn assert_intake_not_found<T: std::fmt::Debug>(what: &str, result: Result<T, OpsError>) {
    match result {
        Err(OpsError::IntakeNotFound { intake_id: 1 }) => {}
        other => panic!("{what}: expected IntakeNotFound for intake 1, got {other:?}"),
    }
}

#[test]
fn listings_are_empty_pages() {
    let fx = Fixture::empty_root();

    let page = list_books(&fx.ops, 10, 0).expect("list_books on a root without a catalog");
    assert!(page.books.is_empty(), "{:?}", page.books);
    assert_eq!(page.total, 0);
    assert!(!page.truncated);

    let page = find_books(&fx.ops, BookFilter::default(), 10, 0)
        .expect("find_books on a root without a catalog");
    assert!(page.books.is_empty(), "{:?}", page.books);
    assert_eq!(page.total, 0);
    assert!(!page.truncated);

    let page =
        list_ocr_pending(&fx.ops, 10, 0).expect("list_ocr_pending on a root without a catalog");
    assert!(page.items.is_empty(), "{:?}", page.items);
    assert_eq!(page.total, 0);
    assert!(!page.truncated);

    fx.assert_nothing_created("listing");
}

#[test]
fn metadata_listings_are_empty_pages() {
    let fx = Fixture::empty_root();

    let page = list_metadata(&fx.ops, MetadataFilter::default(), 10, 0)
        .expect("list_metadata on a root without a catalog");
    assert!(page.rows.is_empty(), "{:?}", page.rows);
    assert_eq!(page.total, 0);
    assert!(!page.truncated);

    let page = list_pending_reviews(&fx.ops, 10, 0)
        .expect("list_pending_reviews on a root without a catalog");
    assert!(page.rows.is_empty(), "{:?}", page.rows);
    assert_eq!(page.total, 0);
    assert!(!page.truncated);

    fx.assert_nothing_created("listing");
}

#[test]
fn aggregates_count_zero() {
    let fx = Fixture::empty_root();

    let stats = show_stats(&fx.ops).expect("stats on a root without a catalog");
    for status in IntakeStatus::ALL {
        assert_eq!(
            stats.intake_counts_by_status.get(status.as_str()),
            Some(&0),
            "status {} must count zero",
            status.as_str()
        );
    }
    assert!(stats.intake_count_by_format.is_empty());
    assert!(stats.book_state_counts_by_stage.is_empty());
    assert!(stats.retrieval_issue_counts_by_status.is_empty());
    assert!(
        stats.papers.is_none(),
        "no papers backend is configured, so the section is absent"
    );

    let counts = category_counts(&fx.ops).expect("categories on a root without a catalog");
    assert!(counts.categories.is_empty(), "{:?}", counts.categories);
    assert_eq!(counts.uncategorised, 0);
    assert_eq!(counts.total, 0);

    fx.assert_nothing_created("aggregating");
}

#[test]
fn per_book_reads_report_the_intake_as_unknown() {
    let fx = Fixture::empty_root();

    assert_intake_not_found("show_book", show_book(&fx.ops, 1));
    assert_intake_not_found("show_toc", show_toc(&fx.ops, 1, &ShowTocArgs::default()));
    assert_intake_not_found("show_metadata_audit", show_metadata_audit(&fx.ops, 1));
    assert_intake_not_found(
        "show_metadata_report",
        show_metadata_report(
            &fx.ops,
            1,
            &AuditData::default_data(),
            &AuditProfile::default(),
        ),
    );
    assert_intake_not_found("show_audit_trail", show_audit_trail(&fx.ops, 1));
    assert_intake_not_found("show_pipeline_trail", show_pipeline_trail(&fx.ops, 1));

    fx.assert_nothing_created("a lookup");
}
