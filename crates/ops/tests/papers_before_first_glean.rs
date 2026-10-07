// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the paper-side reads on a library whose
//! `papers_catalog.db` has not been created yet.
//!
//! The paper catalog is materialised by the first glean, so a library
//! that only ever ingested books has none. The reads must treat that
//! as "no papers" — an empty page, zero counts, an unknown intake —
//! rather than as an open failure, and must not create the file as a
//! side effect of looking.

use std::future::Future;
use std::path::PathBuf;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_core::ItemKind;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_ops::dto::{MetadataFilter, ShowTocArgs};
use bookrack_ops::reads::books::show_stats;
use bookrack_ops::reads::papers::{
    export_csl, fetch_source, list_papers, show_paper, show_paper_toc,
};
use bookrack_ops::reads::papers_metadata::{
    list_paper_metadata, list_paper_pending_reviews, show_paper_audit_trail,
};
use bookrack_ops::{Caller, Ops, OpsError, PapersPaths};
use tempfile::TempDir;

/// A constant-vector embedder, so the fixture opens a warm `Library`
/// without a live embedding service.
struct Fake {
    dim: usize,
}

impl Embedder for Fake {
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
        let (dim, n) = (self.dim, texts.len());
        async move { Ok(vec![vec![0.25f32; dim]; n]) }
    }
}

struct Fixture {
    _tmp: TempDir,
    ops: Ops<Fake>,
    papers_catalog_db: PathBuf,
}

impl Fixture {
    /// A book-side catalog exists; nothing on the paper side does.
    async fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let catalog_db = tmp.path().join("catalog.db");
        Catalog::open(&catalog_db).expect("seed book catalog");

        let papers_catalog_db = tmp.path().join("papers_catalog.db");
        let papers_corpus_db = tmp.path().join("papers_corpus.db");
        let papers_lancedb = tmp.path().join("lancedb_papers");
        let papers_library = bookrack_query::Library::open(
            papers_corpus_db.clone(),
            papers_catalog_db.clone(),
            &papers_lancedb,
            Fake { dim: 8 },
            "fake-model".to_string(),
            5,
            bookrack_glean::CHUNK_VERSION,
        )
        .await
        .expect("open papers library")
        .with_kind(ItemKind::Paper);

        let ops = Ops::catalog_only(
            tmp.path().join("corpus.db"),
            catalog_db,
            &tmp.path().join("lancedb"),
            tmp.path().join("books"),
            tmp.path().join("backup"),
            Caller::cli(),
        )
        .with_papers(
            papers_library,
            PapersPaths {
                corpus_db: papers_corpus_db,
                catalog_db: papers_catalog_db.clone(),
                lancedb_dir: papers_lancedb,
                papers_dir: tmp.path().join("papers"),
            },
        );
        assert!(
            !papers_catalog_db.exists(),
            "fixture precondition: no paper catalog on disk"
        );
        Fixture {
            _tmp: tmp,
            ops,
            papers_catalog_db,
        }
    }
}

fn assert_intake_not_found<T: std::fmt::Debug>(what: &str, result: Result<T, OpsError>) {
    match result {
        Err(OpsError::IntakeNotFound { intake_id: 1 }) => {}
        other => panic!("{what}: expected IntakeNotFound for intake 1, got {other:?}"),
    }
}

#[tokio::test]
async fn list_papers_is_an_empty_page() {
    let fx = Fixture::build().await;
    let page = list_papers(&fx.ops, 10, 0).expect("list_papers on a library without papers");
    assert!(page.papers.is_empty(), "{:?}", page.papers);
    assert_eq!(page.total, 0);
    assert!(!page.truncated);
    assert!(
        !fx.papers_catalog_db.exists(),
        "listing must not create the paper catalog"
    );
}

#[tokio::test]
async fn stats_report_zero_papers_in_every_status() {
    let fx = Fixture::build().await;
    let stats = show_stats(&fx.ops).expect("stats on a library without papers");
    let papers = stats
        .papers
        .expect("papers backend is configured, so the section is present");
    for status in IntakeStatus::ALL {
        assert_eq!(
            papers.intake_counts_by_status.get(status.as_str()),
            Some(&0),
            "status {} must count zero",
            status.as_str()
        );
    }
    assert!(
        !fx.papers_catalog_db.exists(),
        "stats must not create the paper catalog"
    );
}

#[tokio::test]
async fn metadata_listings_are_empty_pages() {
    let fx = Fixture::build().await;
    let page = list_paper_metadata(&fx.ops, MetadataFilter::default(), 10, 0)
        .expect("list_paper_metadata on a library without papers");
    assert!(page.rows.is_empty());
    assert_eq!(page.total, 0);
    assert!(!page.truncated);

    let page = list_paper_pending_reviews(&fx.ops, 10, 0)
        .expect("list_paper_pending_reviews on a library without papers");
    assert!(page.rows.is_empty());
    assert_eq!(page.total, 0);
    assert!(!page.truncated);
    assert!(
        !fx.papers_catalog_db.exists(),
        "listing must not create the paper catalog"
    );
}

#[tokio::test]
async fn per_paper_reads_report_the_intake_as_unknown() {
    let fx = Fixture::build().await;
    assert_intake_not_found("show_paper", show_paper(&fx.ops, 1));
    assert_intake_not_found(
        "show_paper_toc",
        show_paper_toc(&fx.ops, 1, &ShowTocArgs::default()),
    );
    assert_intake_not_found("export_csl", export_csl(&fx.ops, 1));
    assert_intake_not_found("fetch_source", fetch_source(&fx.ops, 1));
    assert_intake_not_found("show_paper_audit_trail", show_paper_audit_trail(&fx.ops, 1));
    assert!(
        !fx.papers_catalog_db.exists(),
        "a lookup must not create the paper catalog"
    );
}
