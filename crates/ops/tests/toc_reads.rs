// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the paginated TOC reads.
//!
//! Each test seeds a tempdir-backed catalog + corpus pair and drives
//! `show_toc` (or `show_paper_toc`) through the ops surface. The focus
//! is on the pagination contract: page chaining through `next_offset`,
//! the filter-wide `total`, the page-size clamp, and reachability of
//! entries past the per-page cap.

use std::future::Future;
use std::path::PathBuf;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::ItemKind;
use bookrack_corpus::{Corpus, NewNode, NodeType};
use bookrack_embed::{Embedder, OllamaEmbedClient, Result as EmbedResult};
use bookrack_ops::dto::{MAX_TOC_NODES, ShowTocArgs, Toc};
use bookrack_ops::reads::books::show_toc;
use bookrack_ops::reads::papers::show_paper_toc;
use bookrack_ops::{Caller, Ops, OpsError, PapersPaths};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
    corpus: Corpus,
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
        let corpus = Corpus::open(&corpus_db).expect("seed corpus");
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
            corpus,
            catalog_db,
        }
    }

    fn register_book(&self, sha: &str) -> i64 {
        let mut catalog = Catalog::open(&self.catalog_db).expect("open catalog");
        catalog
            .register_intake(ItemKind::Book, &NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id
    }
}

/// Seed a book root plus `n` chapters titled `Chapter 0..n` into
/// `corpus` under `intake_id`, each with a distinct document-order
/// span so the TOC walk is root first, then the chapters by index.
fn seed_chapters(corpus: &mut Corpus, intake_id: i64, n: u32) {
    let partition = corpus.allocate_partition(intake_id).expect("partition");
    let root = partition.book_root_id;
    corpus
        .insert_node(
            &NewNode::root(root, NodeType::Work)
                .title("A Book")
                .toc_span(0, i64::from(n) * 10 + 10),
        )
        .expect("root");
    let ids = corpus.allocate_node_ids(partition.idx, n).expect("ids");
    let chapters: Vec<NewNode> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            NewNode::child(*id, root, root, i as i64, 1, NodeType::Chapter)
                .title(format!("Chapter {i}"))
                .toc_span((i as i64) * 10 + 5, (i as i64) * 10 + 9)
        })
        .collect();
    corpus.insert_nodes(&chapters).expect("chapters");
}

/// The titles of one TOC page, in walk order.
fn titles(toc: &Toc) -> Vec<String> {
    toc.nodes
        .iter()
        .map(|n| n.title.clone().unwrap_or_default())
        .collect()
}

#[test]
fn pages_chain_through_the_cursor_until_it_terminates() {
    let mut fx = Fixture::build();
    let intake_id = fx.register_book("sha-toc");
    seed_chapters(&mut fx.corpus, intake_id, 5);

    // Root + 5 chapters = 6 entries, read in pages of two.
    let mut args = ShowTocArgs {
        offset: 0,
        limit: Some(2),
    };
    let mut walked = Vec::new();
    let mut pages = 0;
    loop {
        let page = show_toc(&fx.ops, intake_id, &args).expect("page");
        assert_eq!(page.total, 6);
        assert_eq!(page.truncated, page.next_offset.is_some());
        walked.extend(titles(&page));
        pages += 1;
        match page.next_offset {
            Some(next) => args.offset = next,
            None => break,
        }
    }
    assert_eq!(pages, 3);

    let full = show_toc(&fx.ops, intake_id, &ShowTocArgs::default()).expect("full");
    assert_eq!(full.total, 6);
    assert_eq!(full.next_offset, None);
    assert_eq!(walked, titles(&full), "pages must chain without gaps");
}

#[test]
fn entries_past_the_page_cap_are_reachable_by_offset() {
    let mut fx = Fixture::build();
    let intake_id = fx.register_book("sha-big");
    // Root + MAX_TOC_NODES chapters: one entry more than a default page.
    seed_chapters(&mut fx.corpus, intake_id, MAX_TOC_NODES as u32);

    let first = show_toc(&fx.ops, intake_id, &ShowTocArgs::default()).expect("first page");
    assert_eq!(first.nodes.len(), MAX_TOC_NODES);
    assert_eq!(first.total, MAX_TOC_NODES as u64 + 1);
    assert_eq!(first.next_offset, Some(MAX_TOC_NODES as u32));
    assert!(first.truncated);

    // A limit above the cap clamps back down to it.
    let clamped = show_toc(
        &fx.ops,
        intake_id,
        &ShowTocArgs {
            offset: 0,
            limit: Some(MAX_TOC_NODES as u32 + 500),
        },
    )
    .expect("clamped page");
    assert_eq!(clamped.nodes.len(), MAX_TOC_NODES);

    // The entry past the cap comes back on the second page.
    let second = show_toc(
        &fx.ops,
        intake_id,
        &ShowTocArgs {
            offset: MAX_TOC_NODES as u32,
            limit: None,
        },
    )
    .expect("second page");
    assert_eq!(
        titles(&second),
        vec![format!("Chapter {}", MAX_TOC_NODES - 1)]
    );
    assert_eq!(second.next_offset, None);
    assert!(!second.truncated);
}

#[test]
fn an_intake_without_corpus_nodes_reads_as_an_empty_toc() {
    let fx = Fixture::build();
    let intake_id = fx.register_book("sha-empty");

    let toc = show_toc(&fx.ops, intake_id, &ShowTocArgs::default()).expect("toc");
    assert!(toc.nodes.is_empty());
    assert_eq!(toc.total, 0);
    assert_eq!(toc.next_offset, None);
    assert!(!toc.truncated);
}

#[test]
fn an_unknown_intake_is_intake_not_found() {
    let fx = Fixture::build();
    assert!(matches!(
        show_toc(&fx.ops, 404, &ShowTocArgs::default()),
        Err(OpsError::IntakeNotFound { intake_id: 404 })
    ));
}

/// A constant-vector embedder, so the papers-side fixture can open a
/// warm `Library` without a live embedding service.
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

#[tokio::test]
async fn paper_toc_pages_with_the_same_contract() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let papers_catalog_db = tmp.path().join("papers_catalog.db");
    let papers_corpus_db = tmp.path().join("papers_corpus.db");
    let papers_lancedb = tmp.path().join("lancedb_papers");

    let intake_id = {
        let mut catalog = Catalog::open(&papers_catalog_db).expect("open paper catalog");
        catalog
            .register_intake(ItemKind::Paper, &NewIntake::new("sha-paper"))
            .expect("register intake")
            .into_intake()
            .intake_id
    };
    {
        let mut corpus = Corpus::open(&papers_corpus_db).expect("open paper corpus");
        seed_chapters(&mut corpus, intake_id, 3);
    }

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
            catalog_db: papers_catalog_db,
            lancedb_dir: papers_lancedb,
            papers_dir: tmp.path().join("papers"),
        },
    );

    // Root + 3 chapters = 4 entries, read in pages of two.
    let first = show_paper_toc(
        &ops,
        intake_id,
        &ShowTocArgs {
            offset: 0,
            limit: Some(2),
        },
    )
    .expect("first page");
    assert_eq!(first.total, 4);
    assert_eq!(first.next_offset, Some(2));
    assert!(first.truncated);

    let second = show_paper_toc(
        &ops,
        intake_id,
        &ShowTocArgs {
            offset: 2,
            limit: Some(2),
        },
    )
    .expect("second page");
    assert_eq!(titles(&second), vec!["Chapter 1", "Chapter 2"]);
    assert_eq!(second.next_offset, None);
    assert!(!second.truncated);
}
