// SPDX-License-Identifier: Apache-2.0

//! End-to-end offline test of the query facade: build a synthetic book on
//! disk, index a prose leaf in the vector store, then drive a search
//! through [`Library`] with a stub embedder — no Ollama, no PDFium.

use std::future::Future;

use bookrack_core::NodeType;
use bookrack_corpus::Corpus;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_extract::{
    Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc, TocEntry,
};
use bookrack_ingest::{StructureParams, ingest_structure};
use bookrack_query::Library;
use bookrack_vectors::{ChunkRow, ChunkStore};

const DIM: usize = 4;

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
            },
            Block {
                kind: BlockKind::Body,
                text: "The passage body.".to_string(),
                source_unit: 0,
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
            extractor_version: "test-1".to_string(),
            text_layer_quality: TextLayerQuality::BornDigital,
            skipped_units: Vec::new(),
        },
    }
}

#[tokio::test]
async fn search_returns_a_cited_passage_through_the_facade() {
    let dir = tempfile::tempdir().expect("temp dir");
    let corpus_db = dir.path().join("corpus.db");
    let lancedb_dir = dir.path().join("lancedb");

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
    let library = Library::open(corpus_db, &lancedb_dir, Fixed, 5)
        .await
        .expect("open library");
    assert_eq!(library.dimension(), DIM);

    let hits = library.search("anything", None).await.expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].start_node_id, leaf_id);
    assert_eq!(hits[0].breadcrumb, "A Test Book \u{203a} Chapter One");
}
