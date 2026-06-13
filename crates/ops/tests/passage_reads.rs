// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `read_context` and `read_span`.
//!
//! Both reads serve passage text by structural position. The fixtures
//! seed a corpus file directly: a small book with two chapters plus an
//! empty one for window and boundary behaviour, and a book of
//! oversized leaves for the character-budget and paging behaviour.

use bookrack_catalog::Catalog;
use bookrack_core::KindedNodeId;
use bookrack_corpus::{Corpus, NewNode, NodeId, NodeType};
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::dto::{MAX_CONTEXT_RADIUS, MAX_READ_CHARS};
use bookrack_ops::reads::passages::{read_context, read_span};
use bookrack_ops::{Caller, Ops, OpsError};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
    corpus: Corpus,
}

impl Fixture {
    fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let catalog_db = tmp.path().join("catalog.db");
        let corpus_db = tmp.path().join("corpus.db");
        let lancedb_dir = tmp.path().join("lancedb");
        Catalog::open(&catalog_db).expect("seed catalog");
        let corpus = Corpus::open(&corpus_db).expect("seed corpus");
        let books_dir = tmp.path().join("books");
        let backup_dir = tmp.path().join("backup");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            corpus_db,
            catalog_db,
            &lancedb_dir,
            books_dir,
            backup_dir,
            Caller::cli(),
        );
        Fixture {
            _tmp: tmp,
            ops,
            corpus,
        }
    }
}

/// Ids of the small book's organizing nodes and leaves.
struct SmallBook {
    chapter_one: NodeId,
    chapter_two: NodeId,
    empty_chapter: NodeId,
    /// Leaf ids indexed by document-order position 0..=9.
    leaves: Vec<NodeId>,
}

/// Seed a ten-leaf book under `intake_id`: chapter one spans positions
/// 0..=4, chapter two spans 5..=9 (its position-7 leaf is a table, the
/// rest are paragraphs), and a third chapter carries no leaves at all.
fn seed_small_book(corpus: &mut Corpus, intake_id: i64) -> SmallBook {
    let partition = corpus.allocate_partition(intake_id).expect("partition");
    let root = partition.book_root_id;
    corpus
        .insert_node(
            &NewNode::root(root, NodeType::Work)
                .title("A Small Book")
                .toc_span(0, 9),
        )
        .expect("root");
    let ids = corpus.allocate_node_ids(partition.idx, 13).expect("ids");
    let (chapter_one, chapter_two, empty_chapter) = (ids[0], ids[1], ids[2]);
    corpus
        .insert_node(
            &NewNode::child(chapter_one, root, root, 0, 1, NodeType::Chapter)
                .title("Chapter One")
                .toc_span(0, 4),
        )
        .expect("chapter one");
    corpus
        .insert_node(
            &NewNode::child(chapter_two, root, root, 1, 1, NodeType::Chapter)
                .title("Chapter Two")
                .toc_span(5, 9),
        )
        .expect("chapter two");
    corpus
        .insert_node(
            &NewNode::child(empty_chapter, root, root, 2, 1, NodeType::Chapter)
                .title("Empty Chapter"),
        )
        .expect("empty chapter");
    let mut leaves = Vec::new();
    for pos in 0..10i64 {
        let leaf = ids[3 + pos as usize];
        let parent = if pos < 5 { chapter_one } else { chapter_two };
        let node_type = if pos == 7 {
            NodeType::Table
        } else {
            NodeType::Paragraph
        };
        let text = format!("passage {pos}");
        corpus
            .insert_node(
                &NewNode::child(leaf, parent, root, pos % 5, 2, node_type)
                    .text(text.clone())
                    .text_stats(text.chars().count() as i64, 1)
                    .toc_span(pos, pos),
            )
            .expect("leaf");
        leaves.push(leaf);
    }
    SmallBook {
        chapter_one,
        chapter_two,
        empty_chapter,
        leaves,
    }
}

/// Leaf size that lets exactly three leaves fit [`MAX_READ_CHARS`].
fn big_leaf_chars() -> usize {
    MAX_READ_CHARS / 4 + 1
}

/// Seed a book of ten oversized paragraphs (each [`big_leaf_chars`]
/// long) in one chapter, so character-budget truncation fires after
/// three leaves. Returns the chapter id and the leaf ids in document
/// order.
fn seed_big_book(corpus: &mut Corpus, intake_id: i64) -> (NodeId, Vec<NodeId>) {
    let partition = corpus.allocate_partition(intake_id).expect("partition");
    let root = partition.book_root_id;
    corpus
        .insert_node(
            &NewNode::root(root, NodeType::Work)
                .title("A Big Book")
                .toc_span(0, 9),
        )
        .expect("root");
    let ids = corpus.allocate_node_ids(partition.idx, 11).expect("ids");
    let chapter = ids[0];
    corpus
        .insert_node(
            &NewNode::child(chapter, root, root, 0, 1, NodeType::Chapter)
                .title("The Chapter")
                .toc_span(0, 9),
        )
        .expect("chapter");
    let mut leaves = Vec::new();
    for pos in 0..10i64 {
        let leaf = ids[1 + pos as usize];
        let text = "x".repeat(big_leaf_chars());
        corpus
            .insert_node(
                &NewNode::child(leaf, chapter, root, pos, 2, NodeType::Paragraph)
                    .text(text)
                    .text_stats(big_leaf_chars() as i64, 1)
                    .toc_span(pos, pos),
            )
            .expect("leaf");
        leaves.push(leaf);
    }
    (chapter, leaves)
}

#[test]
fn read_context_centres_the_window_across_chapter_boundaries() {
    let mut fx = Fixture::build();
    let book = seed_small_book(&mut fx.corpus, 1);

    // Anchor at position 5 (chapter two's first leaf); the window
    // reaches back into chapter one because context follows document
    // order, not the organizing tree.
    let window = read_context(&fx.ops, KindedNodeId::book(book.leaves[5]), 2, 2).expect("window");
    assert_eq!(window.intake_id, 1);
    assert_eq!(window.anchor_node_id, book.leaves[5].get());
    let positions: Vec<i64> = window.passages.iter().map(|p| p.toc_position).collect();
    assert_eq!(positions, vec![3, 4, 5, 6, 7]);
    let texts: Vec<&str> = window.passages.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "passage 3",
            "passage 4",
            "passage 5",
            "passage 6",
            "passage 7"
        ]
    );
    assert!(!window.truncated);
}

#[test]
fn read_context_returns_structural_leaves_with_their_kind() {
    let mut fx = Fixture::build();
    let book = seed_small_book(&mut fx.corpus, 1);

    // Position 7 is a table; it is returned in place, tagged by kind.
    let window = read_context(&fx.ops, KindedNodeId::book(book.leaves[6]), 0, 1).expect("window");
    let kinds: Vec<&str> = window
        .passages
        .iter()
        .map(|p| p.node_type.as_str())
        .collect();
    assert_eq!(kinds, vec!["paragraph", "table"]);
}

#[test]
fn read_context_at_the_book_edge_returns_what_exists() {
    let mut fx = Fixture::build();
    let book = seed_small_book(&mut fx.corpus, 1);

    let window = read_context(&fx.ops, KindedNodeId::book(book.leaves[0]), 3, 1).expect("window");
    let positions: Vec<i64> = window.passages.iter().map(|p| p.toc_position).collect();
    assert_eq!(positions, vec![0, 1]);
    assert!(
        !window.truncated,
        "leaves missing because the book ends are not a truncation"
    );
}

#[test]
fn read_context_clamps_the_radius_and_reports_it() {
    let mut fx = Fixture::build();
    let book = seed_small_book(&mut fx.corpus, 1);

    let window = read_context(
        &fx.ops,
        KindedNodeId::book(book.leaves[5]),
        MAX_CONTEXT_RADIUS + 5,
        0,
    )
    .expect("window");
    let positions: Vec<i64> = window.passages.iter().map(|p| p.toc_position).collect();
    assert_eq!(positions, vec![0, 1, 2, 3, 4, 5]);
    assert!(window.truncated, "the radius clamp must be reported");
}

#[test]
fn read_context_budget_keeps_the_leaves_nearest_the_anchor() {
    let mut fx = Fixture::build();
    let (_chapter, leaves) = seed_big_book(&mut fx.corpus, 2);

    // Three big leaves fit the budget: the anchor and its two
    // immediate neighbours survive, the rest of the window drops.
    let window = read_context(&fx.ops, KindedNodeId::book(leaves[5]), 4, 4).expect("window");
    let positions: Vec<i64> = window.passages.iter().map(|p| p.toc_position).collect();
    assert_eq!(positions, vec![4, 5, 6]);
    assert!(window.truncated);
}

#[test]
fn read_context_refuses_non_leaf_anchors() {
    let mut fx = Fixture::build();
    let book = seed_small_book(&mut fx.corpus, 1);

    let err = read_context(&fx.ops, KindedNodeId::book(book.chapter_one), 1, 1)
        .expect_err("organizing anchor");
    assert!(matches!(err, OpsError::NotALeaf { node_id } if node_id == book.chapter_one.get()));

    let err = read_context(&fx.ops, KindedNodeId::book(NodeId::new(999_999_999)), 1, 1)
        .expect_err("unknown node");
    assert!(matches!(err, OpsError::NodeNotFound { node_id } if node_id == 999_999_999));
}

#[test]
fn read_span_reads_a_whole_chapter_in_one_page() {
    let mut fx = Fixture::build();
    let book = seed_small_book(&mut fx.corpus, 1);

    let span = read_span(&fx.ops, KindedNodeId::book(book.chapter_two), None).expect("span");
    assert_eq!(span.intake_id, 1);
    assert_eq!(span.title.as_deref(), Some("Chapter Two"));
    assert_eq!((span.toc_lo, span.toc_hi), (Some(5), Some(9)));
    let positions: Vec<i64> = span.passages.iter().map(|p| p.toc_position).collect();
    assert_eq!(positions, vec![5, 6, 7, 8, 9]);
    assert_eq!(span.next_offset, None);
    assert!(!span.truncated);
}

#[test]
fn read_span_pages_through_an_oversized_chapter() {
    let mut fx = Fixture::build();
    let (chapter, leaves) = seed_big_book(&mut fx.corpus, 2);

    // Three big leaves per page; ten leaves make four pages.
    let mut cursor = None;
    let mut pages = Vec::new();
    loop {
        let span = read_span(&fx.ops, KindedNodeId::book(chapter), cursor).expect("span");
        assert_eq!(span.truncated, span.next_offset.is_some());
        pages.push(
            span.passages
                .iter()
                .map(|p| p.toc_position)
                .collect::<Vec<i64>>(),
        );
        match span.next_offset {
            Some(offset) => cursor = Some(offset),
            None => break,
        }
    }
    assert_eq!(
        pages,
        vec![vec![0, 1, 2], vec![3, 4, 5], vec![6, 7, 8], vec![9]]
    );
    // Every leaf was served exactly once.
    let served: Vec<i64> = pages.into_iter().flatten().collect();
    assert_eq!(served.len(), leaves.len());
}

#[test]
fn read_span_of_an_empty_chapter_is_empty_not_an_error() {
    let mut fx = Fixture::build();
    let book = seed_small_book(&mut fx.corpus, 1);

    let span = read_span(&fx.ops, KindedNodeId::book(book.empty_chapter), None).expect("span");
    assert_eq!(span.title.as_deref(), Some("Empty Chapter"));
    assert_eq!((span.toc_lo, span.toc_hi), (None, None));
    assert!(span.passages.is_empty());
    assert_eq!(span.next_offset, None);
    assert!(!span.truncated);
}

#[test]
fn read_span_refuses_leaf_targets() {
    let mut fx = Fixture::build();
    let book = seed_small_book(&mut fx.corpus, 1);

    let err =
        read_span(&fx.ops, KindedNodeId::book(book.leaves[0]), None).expect_err("leaf target");
    assert!(matches!(err, OpsError::NotOrganizing { node_id } if node_id == book.leaves[0].get()));

    let err = read_span(&fx.ops, KindedNodeId::book(NodeId::new(999_999_999)), None)
        .expect_err("unknown node");
    assert!(matches!(err, OpsError::NodeNotFound { node_id } if node_id == 999_999_999));
}
