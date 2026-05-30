// SPDX-License-Identifier: Apache-2.0

//! search: dense retrieval over the chunks vector store, each hit dressed
//! with a citation breadcrumb joined from the corpus node tree.
//!
//! Retrieval is pure dense at pilot scale: the query is embedded with the
//! same model the chunks were, the store returns the nearest passages by
//! cosine distance, and each hit's `start_node_id` is walked up the
//! corpus tree to build a `Book › Chapter › Section` breadcrumb.
//!
//! The breadcrumb leads with the book title when the root carries one
//! (the common case — `ingest` writes the biblio title onto the root), so
//! the title falls out of the ancestor walk and is never prefixed twice.
//! It is added only when the walk does not already lead with it. Hybrid
//! retrieval (BM25 / full-text) and an approximate index are deferred.

use std::time::Instant;

use bookrack_core::NodeId;
use bookrack_corpus::Corpus;
use bookrack_embed::{Embedder, build_query_input};
use bookrack_vectors::ChunkStore;

/// Separator between breadcrumb segments.
const BREADCRUMB_SEP: &str = " \u{203a} ";

/// Why a `search` operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// The corpus layer reported an error while building a breadcrumb.
    #[error("corpus error: {0}")]
    Corpus(#[from] bookrack_corpus::CorpusError),

    /// The embed client failed to embed the query.
    #[error("embed error: {0}")]
    Embed(#[from] bookrack_embed::EmbedError),

    /// The vector store reported an error.
    #[error("vector store error: {0}")]
    Vectors(#[from] bookrack_vectors::VectorsError),

    /// The embedder returned no vector for the query.
    #[error("the embedder returned no vector for the query")]
    EmptyEmbedding,
}

/// A fallible `search` operation.
pub type Result<T> = std::result::Result<T, SearchError>;

/// One search result: a passage and where to cite it from.
#[derive(Debug, Clone)]
pub struct Citation {
    /// The passage text to display.
    pub text: String,
    /// A `Book \u{203a} Chapter \u{203a} Section` trail of titled
    /// ancestors; empty when no ancestor carries a title.
    pub breadcrumb: String,
    /// The leaf the passage starts in.
    pub start_node_id: NodeId,
    /// Character offset of the passage start within `start_node_id`.
    pub start_char_offset: i32,
    /// The leaf the passage ends in.
    pub end_node_id: NodeId,
    /// Character offset of the passage end within `end_node_id`.
    pub end_char_offset: i32,
    /// SHA-256 of the normalized passage text.
    pub norm_chunk_sha256: String,
    /// Cosine distance to the query — smaller is nearer.
    pub distance: f32,
}

/// Retrieve the `top_k` passages nearest `query`, each with a citation
/// breadcrumb resolved from `corpus`.
#[tracing::instrument(name = "search", skip_all, fields(top_k = top_k))]
pub async fn search<E: Embedder>(
    query: &str,
    corpus: &Corpus,
    store: &ChunkStore,
    embedder: &E,
    top_k: usize,
) -> Result<Vec<Citation>> {
    let input = build_query_input(query);
    let embed_started = Instant::now();
    let vectors = embedder.embed_batch(std::slice::from_ref(&input)).await?;
    let query_vector = vectors.first().ok_or(SearchError::EmptyEmbedding)?;
    tracing::debug!(
        elapsed_ms = embed_started.elapsed().as_secs_f64() * 1e3,
        "embedded query"
    );

    let recall_started = Instant::now();
    let hits = store.search(query_vector, top_k).await?;
    tracing::debug!(
        hits = hits.len(),
        elapsed_ms = recall_started.elapsed().as_secs_f64() * 1e3,
        "recalled nearest passages"
    );

    let breadcrumb_started = Instant::now();
    let mut citations = Vec::with_capacity(hits.len());
    for hit in hits {
        citations.push(Citation {
            breadcrumb: breadcrumb(corpus, hit.start_node_id)?,
            text: hit.text,
            start_node_id: hit.start_node_id,
            start_char_offset: hit.start_char_offset,
            end_node_id: hit.end_node_id,
            end_char_offset: hit.end_char_offset,
            norm_chunk_sha256: hit.norm_chunk_sha256,
            distance: hit.distance,
        });
    }
    tracing::debug!(
        elapsed_ms = breadcrumb_started.elapsed().as_secs_f64() * 1e3,
        "resolved breadcrumbs"
    );
    tracing::info!(hits = citations.len(), "search complete");
    Ok(citations)
}

/// Build the breadcrumb for a leaf by walking its organizing ancestors to
/// the book root, collecting their titles top-down.
///
/// The book root's title — the book title — is the top-most segment when
/// present, so it appears once and is not duplicated. The conditional
/// prefix only fires in the degenerate case where the walk does not
/// already lead with the book title.
fn breadcrumb(corpus: &Corpus, start_node_id: NodeId) -> Result<String> {
    let mut titles = Vec::new();
    let mut book_title = None;
    let mut current = Some(start_node_id);
    while let Some(id) = current {
        let Some(node) = corpus.get_node(id)? else {
            break;
        };
        if node.parent_id.is_none() {
            book_title = node.title.clone();
        }
        if node.node_type.is_organizing()
            && let Some(title) = &node.title
        {
            titles.push(title.clone());
        }
        current = node.parent_id;
    }
    // The walk is leaf-to-root; reverse to read book-to-section.
    titles.reverse();

    if let Some(book) = book_title
        && titles.first() != Some(&book)
    {
        titles.insert(0, book);
    }
    Ok(titles.join(BREADCRUMB_SEP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_core::NodeType;
    use bookrack_embed::Result as EmbedResult;
    use bookrack_extract::{
        Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc, TocEntry,
    };
    use bookrack_ingest::{StructureParams, ingest_structure};
    use bookrack_vectors::ChunkRow;
    use std::future::Future;

    const DIM: usize = 4;

    /// An embedder that returns one fixed query vector, ignoring the text.
    struct FixedQuery {
        vector: Vec<f32>,
    }

    impl Embedder for FixedQuery {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let vector = self.vector.clone();
            let n = texts.len();
            async move { Ok(vec![vector; n]) }
        }
    }

    /// A one-chapter extraction; `title` becomes the biblio title.
    fn extraction(title: Option<&str>, with_chapter: bool) -> Extraction {
        let (blocks, entries) = if with_chapter {
            (
                vec![
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
                vec![TocEntry {
                    label: "Chapter One".to_string(),
                    depth: 0,
                    start_block: Some(0),
                }],
            )
        } else {
            (
                vec![Block {
                    kind: BlockKind::Body,
                    text: "The passage body.".to_string(),
                    source_unit: 0,
                }],
                Vec::new(),
            )
        };
        Extraction {
            blocks,
            toc: Toc { entries },
            biblio: Biblio {
                title: title.map(str::to_string),
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

    /// Ingest one synthetic book into an in-memory corpus and index its
    /// first prose leaf in a fresh store under a temp directory, returning
    /// the pieces a search needs plus the indexed leaf id.
    async fn fixture(
        title: Option<&str>,
        with_chapter: bool,
    ) -> (tempfile::TempDir, Corpus, ChunkStore, NodeId) {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let report = ingest_structure(
            &mut corpus,
            1,
            NodeType::Work,
            &extraction(title, with_chapter),
            &StructureParams::default(),
        )
        .expect("structure");

        let leaf = corpus
            .book_nodes(report.book_root_id)
            .expect("nodes")
            .into_iter()
            .find(|n| n.node_type.is_prose_leaf())
            .expect("a prose leaf");

        let dir = tempfile::tempdir().expect("temp dir");
        let store = ChunkStore::open(dir.path(), DIM).await.expect("store");
        store
            .append(&[ChunkRow {
                vector: vec![1.0, 0.0, 0.0, 0.0],
                text: leaf.text_content.clone().unwrap(),
                start_node_id: leaf.node_id,
                start_char_offset: 0,
                end_node_id: leaf.node_id,
                end_char_offset: 100,
                norm_chunk_sha256: "sha".to_string(),
            }])
            .await
            .expect("append");
        (dir, corpus, store, leaf.node_id)
    }

    #[tokio::test]
    async fn breadcrumb_leads_with_the_book_title_once() {
        let (_dir, corpus, store, leaf) = fixture(Some("A Test Book"), true).await;
        let query = FixedQuery {
            vector: vec![1.0, 0.0, 0.0, 0.0],
        };
        let hits = search("anything", &corpus, &store, &query, 5)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_node_id, leaf);
        // The book title appears exactly once, then the chapter.
        assert_eq!(hits[0].breadcrumb, "A Test Book \u{203a} Chapter One");
    }

    #[tokio::test]
    async fn breadcrumb_is_empty_when_no_ancestor_has_a_title() {
        // No biblio title and no TOC: the leaf hangs directly off an
        // untitled root, so there is nothing to cite.
        let (_dir, corpus, store, _leaf) = fixture(None, false).await;
        let query = FixedQuery {
            vector: vec![1.0, 0.0, 0.0, 0.0],
        };
        let hits = search("anything", &corpus, &store, &query, 5)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].breadcrumb.is_empty());
    }
}
