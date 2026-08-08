// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the source-side provenance the paper reads
//! project out of the `intake` row, mirroring `book_source_fields.rs`
//! for the paper pipeline.
//!
//! Every test asserts against the serialized wire shape rather than the
//! DTO struct fields: a field dropped from the projection then fails an
//! assertion instead of breaking the build, and the JSON key names —
//! which are the actual contract — are pinned along with the values.
//! Each pair covers a populated row and a row whose source columns were
//! never written, so an absent column is pinned to `null` rather than to
//! a zero or an empty string.

use std::future::Future;
use std::path::PathBuf;

use bookrack_catalog::{Catalog, NewIntake, NewOverride, NewPublicationAttrs};
use bookrack_core::ItemKind;
use bookrack_corpus::Corpus;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_ops::dto::PaperFilter;
use bookrack_ops::reads::papers::{find_papers, show_paper};
use bookrack_ops::{Caller, Ops, PapersPaths};
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
    async fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let papers_catalog_db = tmp.path().join("papers_catalog.db");
        let papers_corpus_db = tmp.path().join("papers_corpus.db");
        let papers_lancedb = tmp.path().join("lancedb_papers");
        Catalog::open(&papers_catalog_db).expect("seed paper catalog");
        Corpus::open(&papers_corpus_db).expect("seed paper corpus");

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
            tmp.path().join("catalog.db"),
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

        Fixture {
            _tmp: tmp,
            ops,
            papers_catalog_db,
        }
    }

    /// Register one paper intake carrying every source-side column the
    /// reads are expected to surface. `page_count` is not a
    /// registration-time column, so it is written separately.
    fn seed_paper(&self, sha: &str, original_path: &str, byte_size: i64, page_count: i64) -> i64 {
        let mut catalog = Catalog::open(&self.papers_catalog_db).expect("open paper catalog");
        let intake_id = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new(sha)
                    .original_path(original_path)
                    .byte_size(byte_size),
            )
            .expect("register intake")
            .into_intake()
            .intake_id;
        catalog
            .set_page_count(ItemKind::Paper, intake_id, page_count)
            .expect("set page count");
        intake_id
    }

    /// Register one paper intake with nothing but its hash, so every
    /// source-side column stays unwritten.
    fn seed_bare_paper(&self, sha: &str) -> i64 {
        let mut catalog = Catalog::open(&self.papers_catalog_db).expect("open paper catalog");
        catalog
            .register_intake(ItemKind::Paper, &NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id
    }

    /// Register one paper carrying the bibliographic columns the
    /// filters read, then record the curator's corrections to them.
    /// `None` in an override position is the explicit NULL that
    /// removes the field.
    fn seed_curated_paper(
        &self,
        sha: &str,
        base: (&str, &str),
        curated: (Option<&str>, Option<&str>),
    ) -> i64 {
        let mut catalog = Catalog::open(&self.papers_catalog_db).expect("open paper catalog");
        let intake_id = catalog
            .register_intake(ItemKind::Paper, &NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id;
        let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Paper);
        attrs.title = Some(base.0.to_string());
        attrs.year = Some(base.1.to_string());
        catalog
            .upsert_publication_attrs(&attrs)
            .expect("seed paper attrs");
        for (field, value) in [("title", curated.0), ("year", curated.1)] {
            catalog
                .set_override(&NewOverride::new(
                    intake_id,
                    ItemKind::Paper,
                    field,
                    value.map(str::to_string),
                    "human",
                ))
                .expect("write override");
        }
        intake_id
    }
}

/// Ids `find_papers` returns for a filter, with the page and the total
/// held to the same set.
fn papers_matching(fx: &Fixture, filter: PaperFilter) -> Vec<i64> {
    let page = find_papers(&fx.ops, filter, 100, 0).expect("find");
    let ids: Vec<i64> = page.papers.iter().map(|p| p.intake_id).collect();
    assert_eq!(
        page.total as usize,
        ids.len(),
        "`total` and the page disagree about how many papers match"
    );
    ids
}

#[tokio::test]
async fn find_papers_matches_the_bibliography_it_reports() {
    // Every bibliographic field a paper row reports is the curated one,
    // so a filter reading the extracted values answers with rows whose
    // reported values do not match what was asked for. `year` is an
    // equality match, so it cannot be satisfied by a coincidental
    // substring the way a title can.
    let fx = Fixture::build().await;
    let paper = fx.seed_curated_paper(
        "sha-curated",
        ("A Survey of Widgt Design", "2016"),
        (Some("A Survey of Widget Design"), Some("2017")),
    );

    assert_eq!(
        papers_matching(
            &fx,
            PaperFilter {
                title_substring: Some("Widget Design".to_string()),
                ..PaperFilter::default()
            }
        ),
        vec![paper],
        "the corrected title does not answer the filter"
    );
    assert_eq!(
        papers_matching(
            &fx,
            PaperFilter {
                year: Some("2017".to_string()),
                ..PaperFilter::default()
            }
        ),
        vec![paper],
        "the corrected year does not answer the filter"
    );
    assert_eq!(
        papers_matching(
            &fx,
            PaperFilter {
                year: Some("2016".to_string()),
                ..PaperFilter::default()
            }
        ),
        Vec::<i64>::new(),
        "the replaced year still answers the filter"
    );
}

#[tokio::test]
async fn find_papers_does_not_match_a_field_the_user_removed() {
    let fx = Fixture::build().await;
    fx.seed_curated_paper("sha-nulled", ("Provisional Title", "2020"), (None, None));

    assert_eq!(
        papers_matching(
            &fx,
            PaperFilter {
                title_substring: Some("Provisional".to_string()),
                ..PaperFilter::default()
            }
        ),
        Vec::<i64>::new(),
        "a title the user deleted still answers the filter"
    );
}

#[tokio::test]
async fn list_rows_name_the_source_file() {
    let fx = Fixture::build().await;
    let intake_id = fx.seed_paper("sha-summary", "inbox/2020/a-paper.pdf", 4096, 14);

    let page = find_papers(&fx.ops, PaperFilter::default(), 10, 0).expect("find");
    let row = page
        .papers
        .iter()
        .find(|p| p.intake_id == intake_id)
        .expect("row");
    let value = serde_json::to_value(row).expect("serialize");

    // The basename, not the recorded path: a list row identifies the
    // file a paper came from without carrying a full path per entry.
    assert_eq!(
        value["source_filename"], "a-paper.pdf",
        "a list row must name its source file, so a paper whose title the \
         IDENTIFY pass failed to extract can still be identified"
    );
    assert!(
        value.get("source_path").is_none(),
        "the full path stays in the detail read; list rows carry the basename only"
    );
}

#[tokio::test]
async fn a_list_row_without_a_recorded_path_carries_a_null_filename() {
    let fx = Fixture::build().await;
    let intake_id = fx.seed_bare_paper("sha-pathless");

    let page = find_papers(&fx.ops, PaperFilter::default(), 10, 0).expect("find");
    let row = page
        .papers
        .iter()
        .find(|p| p.intake_id == intake_id)
        .expect("row");
    let value = serde_json::to_value(row).expect("serialize");
    // Present and null, not absent: a client reading the key learns the
    // path was never recorded rather than that the read dropped it.
    assert!(
        value
            .get("source_filename")
            .is_some_and(serde_json::Value::is_null),
        "an intake registered without an original path has no filename to report"
    );
}

#[tokio::test]
async fn the_detail_read_carries_the_source_record_of_its_intake_row() {
    let fx = Fixture::build().await;
    let intake_id = fx.seed_paper("sha-detail", "inbox/2020/another-paper.pdf", 8192, 22);

    let detail = show_paper(&fx.ops, intake_id).expect("show");
    let value = serde_json::to_value(&detail).expect("serialize");

    assert_eq!(
        value["source_path"], "inbox/2020/another-paper.pdf",
        "the detail read reports the path recorded at intake, verbatim"
    );
    assert_eq!(value["source_filename"], "another-paper.pdf");
    assert_eq!(
        value["source_sha256"], "sha-detail",
        "the detail read reports the hash captured at registration"
    );
    assert_eq!(value["page_count"], 22);
    assert_eq!(value["byte_size"], 8192);
    assert!(
        value["intake_at"].as_str().is_some_and(|at| !at.is_empty()),
        "the detail read reports when the file was first registered"
    );
}

#[tokio::test]
async fn the_detail_read_reports_absent_source_columns_as_null() {
    let fx = Fixture::build().await;
    let intake_id = fx.seed_bare_paper("sha-bare");

    let detail = show_paper(&fx.ops, intake_id).expect("show");
    let value = serde_json::to_value(&detail).expect("serialize");

    // Present and null, not absent: each key is on the wire carrying
    // `null`, so a client can tell an unrecorded column from a field the
    // read never projected. A paper ingested before a column existed, or
    // one whose source was not archived, reads as null rather than as a
    // zero or an empty string.
    for key in ["source_path", "source_filename", "page_count", "byte_size"] {
        assert!(
            value.get(key).is_some_and(serde_json::Value::is_null),
            "{key} must be present and null on an intake with no source columns, got {:?}",
            value.get(key)
        );
    }
}
