// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the paper-side filters that reach the catalog
//! through fields other than the bibliographic ones: the lifecycle
//! `statuses` set and the `contributor_role` qualifier.
//!
//! Every filter test seeds at least two papers, one of which must not
//! match: a single-row fixture passes whether or not the predicate was
//! ever bound, so it proves nothing about the filter.

use std::future::Future;
use std::path::PathBuf;

use bookrack_catalog::{Catalog, IntakeStatus, NewContributor, NewIntake, NewPublicationAttrs};
use bookrack_core::ItemKind;
use bookrack_corpus::Corpus;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_ops::dto::PaperFilter;
use bookrack_ops::reads::papers::find_papers;
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

    /// Register one paper intake and move it to `status`.
    fn seed_paper_at(&self, sha: &str, status: IntakeStatus) -> i64 {
        let mut catalog = Catalog::open(&self.papers_catalog_db).expect("open paper catalog");
        let intake_id = catalog
            .register_intake(ItemKind::Paper, &NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id;
        catalog
            .set_intake_status(ItemKind::Paper, intake_id, status)
            .expect("set intake status");
        intake_id
    }

    /// Register one paper carrying a stored language, so a language
    /// filter has two rows to tell apart.
    fn seed_paper_in_language(&self, sha: &str, language: &str) -> i64 {
        let mut catalog = Catalog::open(&self.papers_catalog_db).expect("open paper catalog");
        let intake_id = catalog
            .register_intake(ItemKind::Paper, &NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id;
        let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Paper);
        attrs.language = Some(language.to_string());
        catalog
            .upsert_publication_attrs(&attrs)
            .expect("seed attrs");
        intake_id
    }

    /// Register one paper carrying a single contributor in `role`.
    fn seed_paper_with_contributor(&self, sha: &str, name: &str, role: &str) -> i64 {
        let mut catalog = Catalog::open(&self.papers_catalog_db).expect("open paper catalog");
        let intake_id = catalog
            .register_intake(ItemKind::Paper, &NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id;
        catalog
            .add_contributor(&NewContributor::new(
                intake_id,
                ItemKind::Paper,
                role,
                0,
                "extracted",
                name,
            ))
            .expect("add contributor");
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
async fn statuses_select_one_lifecycle_state_out_of_two() {
    let fx = Fixture::build().await;
    let extracted = fx.seed_paper_at("sha-extracted", IntakeStatus::Extracted);
    let embedded = fx.seed_paper_at("sha-embedded", IntakeStatus::Embedded);

    let matched = papers_matching(
        &fx,
        PaperFilter {
            statuses: vec![IntakeStatus::Embedded],
            ..PaperFilter::default()
        },
    );

    assert_eq!(
        matched,
        vec![embedded],
        "only the embedded paper may match; the extracted one ({extracted}) must be filtered out"
    );
}

#[tokio::test]
async fn contributor_role_narrows_a_name_shared_by_two_papers() {
    let fx = Fixture::build().await;
    let authored = fx.seed_paper_with_contributor("sha-authored", "Wren Halloway", "author");
    let edited = fx.seed_paper_with_contributor("sha-edited", "Wren Halloway", "editor");

    let matched = papers_matching(
        &fx,
        PaperFilter {
            contributor_name: Some("Wren Halloway".to_string()),
            contributor_role: Some("editor".to_string()),
            ..PaperFilter::default()
        },
    );

    assert_eq!(
        matched,
        vec![edited],
        "the role must narrow the name match; the authored paper ({authored}) must be filtered out"
    );
}

/// `contributor_role` is bound inside the `contributor_name` branch, so
/// on its own it is ignored rather than refused. This pins the current
/// behaviour: changing it means deciding between refusing the lone
/// qualifier and letting the role filter stand alone.
#[tokio::test]
async fn contributor_role_alone_is_ignored() {
    let fx = Fixture::build().await;
    let authored = fx.seed_paper_with_contributor("sha-authored", "Wren Halloway", "author");
    let edited = fx.seed_paper_with_contributor("sha-edited", "Wren Halloway", "editor");

    let matched = papers_matching(
        &fx,
        PaperFilter {
            contributor_role: Some("editor".to_string()),
            ..PaperFilter::default()
        },
    );

    assert_eq!(
        matched,
        vec![authored, edited],
        "without a contributor name the role qualifier takes no effect"
    );
}

#[tokio::test]
async fn language_selects_one_paper_out_of_two() {
    let fx = Fixture::build().await;
    let german = fx.seed_paper_in_language("sha-de", "de");
    let latin = fx.seed_paper_in_language("sha-la", "la");

    let matched = papers_matching(
        &fx,
        PaperFilter {
            language: vec!["de".to_string()],
            ..PaperFilter::default()
        },
    );

    assert_eq!(
        matched,
        vec![german],
        "the latin paper ({latin}) must not answer a german filter"
    );
}
