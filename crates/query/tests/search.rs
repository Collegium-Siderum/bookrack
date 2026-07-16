// SPDX-License-Identifier: Apache-2.0

//! End-to-end offline test of the query facade: build a synthetic book on
//! disk, index a prose leaf in the vector store, then drive a search
//! through [`Library`] with a stub embedder — no Ollama, no PDFium.

use std::future::Future;
use std::path::Path;

use bookrack_catalog::Catalog;
use bookrack_core::{ItemKind, NodeType};
use bookrack_corpus::Corpus;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_extract::{
    Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc, TocEntry,
};
use bookrack_ingest::{StructureParams, current_index_stamps, ingest_structure};
use bookrack_query::Library;
use bookrack_vectors::{ChunkRow, ChunkStore};

/// Bring `catalog.db` into existence at the current schema so the
/// facade's read-only opens succeed. The facade refuses to open a
/// non-existent catalog file.
fn seed_catalog(catalog_db: &Path) {
    drop(Catalog::open(catalog_db).expect("seed catalog"));
}

const DIM: usize = 4;
const MODEL: &str = "test-model";

/// An embedder that returns one fixed vector per input, ignoring the text.
/// It serves both the dimension probe and the query embedding.
struct Fixed;

impl Embedder for Fixed {
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
        let n = texts.len();
        async move { Ok(vec![vec![1.0, 0.0, 0.0, 0.0]; n]) }
    }
}

/// A one-chapter extraction whose biblio title becomes the book root title.
fn extraction() -> Extraction {
    Extraction {
        blocks: vec![
            Block {
                kind: BlockKind::Heading { level: 1 },
                text: "Chapter One".to_string(),
                source_unit: 0,
                style: None,
            },
            Block {
                kind: BlockKind::Body,
                text: "The passage body.".to_string(),
                source_unit: 0,
                style: None,
            },
        ],
        toc: Toc {
            entries: vec![TocEntry {
                label: "Chapter One".to_string(),
                depth: 0,
                start_block: Some(0),
            }],
        },
        biblio: Biblio {
            title: Some("A Test Book".to_string()),
            ..Default::default()
        },
        provenance: Provenance {
            adapter: "test".to_string(),
            extractor_version: 1,
            text_layer_quality: TextLayerQuality::BornDigital,
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks: Vec::new(),
        },
    }
}

#[tokio::test]
async fn search_returns_a_cited_passage_through_the_facade() {
    let dir = tempfile::tempdir().expect("temp dir");
    let corpus_db = dir.path().join("corpus.db");
    let catalog_db = dir.path().join("catalog.db");
    let lancedb_dir = dir.path().join("lancedb");
    seed_catalog(&catalog_db);

    // Build a one-chapter book in an on-disk corpus.
    let leaf_id = {
        let mut corpus = Corpus::open(&corpus_db).expect("open corpus");
        let report = ingest_structure(
            &mut corpus,
            1,
            NodeType::Work,
            &extraction(),
            &StructureParams::default(),
        )
        .expect("structure");
        // Stamp the index with the model and dimension it is built at, so
        // the facade's serve-side gate admits it.
        corpus
            .reconcile_index_stamps(&current_index_stamps(MODEL, DIM as u32))
            .expect("stamp");
        let leaf = corpus
            .book_nodes(report.book_root_id)
            .expect("nodes")
            .into_iter()
            .find(|n| n.node_type.is_prose_leaf())
            .expect("a prose leaf");

        // Index the leaf in the vector store, then close both handles so
        // the facade reopens them from their paths.
        let store = ChunkStore::open(&lancedb_dir, DIM)
            .await
            .expect("open store");
        store
            .append(&[ChunkRow {
                vector: vec![1.0, 0.0, 0.0, 0.0],
                text: leaf.text_content.clone().expect("leaf text"),
                start_node_id: leaf.node_id,
                start_char_offset: 0,
                end_node_id: leaf.node_id,
                end_char_offset: 100,
                norm_chunk_sha256: "sha".to_string(),
            }])
            .await
            .expect("append");
        leaf.node_id
    };

    // The facade probes the dimension, opens the store, and reopens a
    // read-only corpus per search call.
    let library = Library::open(
        corpus_db,
        catalog_db,
        &lancedb_dir,
        Fixed,
        MODEL.to_string(),
        5,
        bookrack_ingest::CHUNK_VERSION,
    )
    .await
    .expect("open library");
    assert_eq!(library.dimension(), DIM);

    let hits = library.search("anything", None).await.expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].start_node_id, leaf_id);
    assert_eq!(hits[0].breadcrumb, "A Test Book \u{203a} Chapter One");
}

/// Append a single stamped chunk under `corpus_db` / `lancedb_dir`, built
/// with `MODEL` at `DIM`. Shared setup for the gate tests below.
async fn build_stamped_index(corpus_db: &std::path::Path, lancedb_dir: &std::path::Path) {
    let mut corpus = Corpus::open(corpus_db).expect("open corpus");
    let report = ingest_structure(
        &mut corpus,
        1,
        NodeType::Work,
        &extraction(),
        &StructureParams::default(),
    )
    .expect("structure");
    corpus
        .reconcile_index_stamps(&current_index_stamps(MODEL, DIM as u32))
        .expect("stamp");
    let leaf = corpus
        .book_nodes(report.book_root_id)
        .expect("nodes")
        .into_iter()
        .find(|n| n.node_type.is_prose_leaf())
        .expect("a prose leaf");
    let store = ChunkStore::open(lancedb_dir, DIM)
        .await
        .expect("open store");
    store
        .append(&[ChunkRow {
            vector: vec![1.0, 0.0, 0.0, 0.0],
            text: leaf.text_content.clone().expect("leaf text"),
            start_node_id: leaf.node_id,
            start_char_offset: 0,
            end_node_id: leaf.node_id,
            end_char_offset: 100,
            norm_chunk_sha256: "sha".to_string(),
        }])
        .await
        .expect("append");
}

#[tokio::test]
async fn opening_with_a_different_model_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let corpus_db = dir.path().join("corpus.db");
    let catalog_db = dir.path().join("catalog.db");
    let lancedb_dir = dir.path().join("lancedb");
    seed_catalog(&catalog_db);
    build_stamped_index(&corpus_db, &lancedb_dir).await;

    // The index was stamped with MODEL; opening it to serve with another
    // model is refused before any query runs.
    let result = Library::open(
        corpus_db,
        catalog_db,
        &lancedb_dir,
        Fixed,
        "other-model".to_string(),
        5,
        bookrack_ingest::CHUNK_VERSION,
    )
    .await;
    assert!(matches!(
        result,
        Err(bookrack_query::QueryError::Corpus(
            bookrack_corpus::CorpusError::IndexStampMismatch { .. }
        ))
    ));
}

#[tokio::test]
async fn an_empty_index_is_served_without_stamps() {
    let dir = tempfile::tempdir().expect("temp dir");
    let corpus_db = dir.path().join("corpus.db");
    let catalog_db = dir.path().join("catalog.db");
    let lancedb_dir = dir.path().join("lancedb");
    seed_catalog(&catalog_db);

    // No book ingested, no stamps written: an empty index has no provenance
    // to check, so the facade opens it without complaint.
    let library = Library::open(
        corpus_db,
        catalog_db,
        &lancedb_dir,
        Fixed,
        MODEL.to_string(),
        5,
        bookrack_ingest::CHUNK_VERSION,
    )
    .await
    .expect("open empty library");
    assert_eq!(library.dimension(), DIM);
    let hits = library.search("anything", None).await.expect("search");
    assert!(hits.is_empty());
}

#[tokio::test]
async fn show_book_and_show_toc_round_trip_through_the_facade() {
    use bookrack_catalog::{NewIntake, NewPublicationAttrs};

    let dir = tempfile::tempdir().expect("temp dir");
    let corpus_db = dir.path().join("corpus.db");
    let catalog_db = dir.path().join("catalog.db");
    let lancedb_dir = dir.path().join("lancedb");

    // Seed an ingested book with a title, then seed the catalog
    // separately with the matching biblio so show_book can find it.
    {
        let mut corpus = Corpus::open(&corpus_db).expect("open corpus");
        ingest_structure(
            &mut corpus,
            1,
            NodeType::Work,
            &extraction(),
            &StructureParams::default(),
        )
        .expect("structure");
        corpus
            .reconcile_index_stamps(&current_index_stamps(MODEL, DIM as u32))
            .expect("stamp");
    }
    {
        let mut catalog = Catalog::open(&catalog_db).expect("open catalog");
        catalog
            .register_intake(ItemKind::Book, &NewIntake::new("sha-1").format("epub"))
            .expect("register");
        let mut attrs = NewPublicationAttrs::new(1, ItemKind::Book);
        attrs.title = Some("A Test Book".to_string());
        catalog.upsert_publication_attrs(&attrs).expect("attrs");
    }

    let library = Library::open(
        corpus_db,
        catalog_db,
        &lancedb_dir,
        Fixed,
        MODEL.to_string(),
        5,
        bookrack_ingest::CHUNK_VERSION,
    )
    .await
    .expect("open library");

    let detail = library.show_book(1).expect("show book").expect("present");
    assert_eq!(detail.intake_id, 1);
    assert_eq!(detail.title.as_deref(), Some("A Test Book"));
    assert_eq!(detail.format.as_deref(), Some("epub"));

    assert!(library.show_book(404).expect("missing").is_none());

    let toc_args = bookrack_query::dto::ShowTocArgs::default();
    let toc = library
        .show_toc(1, &toc_args)
        .expect("show toc")
        .expect("present");
    let bookrack_query::dto::TocNodes::Full(nodes) = &toc.nodes else {
        panic!("default projection must carry full nodes");
    };
    let titles: Vec<&str> = nodes.iter().filter_map(|n| n.title.as_deref()).collect();
    assert!(
        titles.iter().any(|t| t.contains("Chapter")),
        "expected the chapter in the TOC: {titles:?}"
    );
    assert_eq!(toc.total, nodes.len() as u64);
    assert_eq!(toc.next_offset, None);
    assert!(!toc.truncated);
    let stats = detail.toc_stats.expect("toc stats present");
    assert_eq!(stats.entry_count, toc.total);

    let slim = library
        .show_toc(
            1,
            &bookrack_query::dto::ShowTocArgs {
                titles_only: true,
                ..bookrack_query::dto::ShowTocArgs::default()
            },
        )
        .expect("show slim toc")
        .expect("present");
    assert!(matches!(slim.nodes, bookrack_query::dto::TocNodes::Slim(_)));
    assert_eq!(slim.total, toc.total);

    assert!(library.show_toc(404, &toc_args).expect("missing").is_none());
}

#[tokio::test]
async fn list_books_clamps_to_max_list_limit() {
    use bookrack_catalog::NewIntake;
    use bookrack_query::dto::MAX_LIST_LIMIT;

    let dir = tempfile::tempdir().expect("temp dir");
    let corpus_db = dir.path().join("corpus.db");
    let catalog_db = dir.path().join("catalog.db");
    let lancedb_dir = dir.path().join("lancedb");

    {
        let mut catalog = Catalog::open(&catalog_db).expect("open catalog");
        // Two intakes: one row fits even under the clamp, so the clamp
        // engaging is not enough to mark the page as truncated; another
        // row is needed to overflow the clamped page.
        let limit_plus_one = MAX_LIST_LIMIT as usize + 1;
        for n in 0..limit_plus_one {
            catalog
                .register_intake(
                    ItemKind::Book,
                    &NewIntake::new(format!("sha-{n}")).format("epub"),
                )
                .expect("register");
        }
    }

    let library = Library::open(
        corpus_db,
        catalog_db,
        &lancedb_dir,
        Fixed,
        MODEL.to_string(),
        5,
        bookrack_ingest::CHUNK_VERSION,
    )
    .await
    .expect("open library");
    let page = library
        .list_books(MAX_LIST_LIMIT + 100, 0)
        .expect("list books");
    assert_eq!(page.total, MAX_LIST_LIMIT as u64 + 1);
    assert_eq!(page.books.len(), MAX_LIST_LIMIT as usize);
    assert!(
        page.truncated,
        "the clamped page does not cover the full total"
    );

    // Second page picks up the row the clamp held back.
    let tail = library
        .list_books(MAX_LIST_LIMIT, MAX_LIST_LIMIT)
        .expect("list tail");
    assert_eq!(tail.total, MAX_LIST_LIMIT as u64 + 1);
    assert_eq!(tail.books.len(), 1);
    assert!(!tail.truncated, "the second page exhausts the filter");
}

/// Walk `root` and remove the first regular file under any `*.lance/data/`
/// directory. Returns the removed path for diagnostics; panics if no such
/// file exists. Used by the missing-fragment test below to simulate a
/// vector store whose backing data has been disturbed out-of-band.
fn remove_one_lance_data_file(root: &Path) -> std::path::PathBuf {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "data")
                && path
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".lance"))
            {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut candidates = Vec::new();
    walk(root, &mut candidates).expect("walk lancedb dir");
    let chosen = candidates
        .into_iter()
        .next()
        .expect("at least one fragment file under chunks.lance/data");
    std::fs::remove_file(&chosen).expect("remove fragment");
    chosen
}

#[tokio::test]
async fn search_returns_a_readable_error_when_a_lance_data_file_is_missing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let corpus_db = dir.path().join("corpus.db");
    let catalog_db = dir.path().join("catalog.db");
    let lancedb_dir = dir.path().join("lancedb");
    seed_catalog(&catalog_db);
    build_stamped_index(&corpus_db, &lancedb_dir).await;

    let removed = remove_one_lance_data_file(&lancedb_dir);
    assert!(!removed.exists(), "fragment must be gone before opening");

    // Opening may succeed (the manifest is still readable); the failure
    // surfaces when the search actually scans the fragment. Either outcome
    // must be a readable error, never a panic.
    let outcome = match Library::open(
        corpus_db,
        catalog_db,
        &lancedb_dir,
        Fixed,
        MODEL.to_string(),
        5,
        bookrack_ingest::CHUNK_VERSION,
    )
    .await
    {
        Ok(library) => library.search("anything", None).await.err(),
        Err(err) => Some(err),
    };
    let err = outcome.expect("a disturbed fragment must surface an error");
    let chain = format!("{err:#}");
    assert!(
        matches!(
            err,
            bookrack_query::QueryError::Vectors(_) | bookrack_query::QueryError::Search(_),
        ),
        "expected a vectors / search error, got: {chain}",
    );
}
