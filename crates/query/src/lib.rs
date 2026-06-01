// SPDX-License-Identifier: Apache-2.0

//! bookrack-query: the read-only query facade.
//!
//! A single capability surface over the corpus and vector store that
//! query consumers — the MCP server, the CLI — call without touching the
//! database crates or their schema. Consumers depend only on this crate;
//! the `corpus` / `vectors` / `search` handles and their field shapes stay
//! behind it. Adding or removing a capability is adding or removing one
//! method here.

use std::path::{Path, PathBuf};

use bookrack_catalog::Catalog;
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_search::{cite, retrieve};
use bookrack_vectors::ChunkStore;

// Re-exported so consumers name query results through this crate, not the
// crates behind the facade.
pub use bookrack_core::NodeId;
pub use bookrack_search::Citation;

/// Why a query operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// The embedder failed while embedding the dimension probe.
    #[error("embed error: {0}")]
    Embed(#[from] bookrack_embed::EmbedError),

    /// The vector store could not be opened or queried.
    #[error("vector store error: {0}")]
    Vectors(#[from] bookrack_vectors::VectorsError),

    /// A read-only corpus handle could not be opened.
    #[error("corpus error: {0}")]
    Corpus(#[from] bookrack_corpus::CorpusError),

    /// The catalog database could not be opened or queried.
    #[error("catalog error: {0}")]
    Catalog(#[from] bookrack_catalog::CatalogError),

    /// The underlying search operation failed.
    #[error("search error: {0}")]
    Search(#[from] bookrack_search::SearchError),

    /// The embedder returned no vector for the dimension probe.
    #[error("the embedder returned no vector for the dimension probe")]
    EmptyProbe,
}

/// A fallible query operation.
pub type Result<T> = std::result::Result<T, QueryError>;

/// Warm, shareable query state.
///
/// Holds the vector store and embedder open for the process lifetime. A
/// read-only corpus handle is opened per call instead, because the
/// underlying SQLite connection is not `Sync`; opening an existing
/// database file is cheap, and it keeps this type free to be shared across
/// concurrent requests behind an `Arc`.
pub struct Library<E: Embedder> {
    store: ChunkStore,
    embedder: E,
    corpus_db: PathBuf,
    catalog_db: PathBuf,
    default_top_k: usize,
}

impl<E: Embedder> Library<E> {
    /// Open the warm state: probe the embedder for its vector width, then
    /// open the vector store at that width. The store's dimension is fixed
    /// at creation and must match the embedding model.
    ///
    /// `embed_model` is the model this daemon is configured to serve with.
    /// A non-empty index is verified against the build stamps it carries:
    /// serving an index built with a different model or a stale algorithm
    /// version is refused here, at startup, rather than returning subtly
    /// wrong results. An empty index has no provenance to check, so it is
    /// served without complaint.
    pub async fn open(
        corpus_db: PathBuf,
        catalog_db: PathBuf,
        lancedb_dir: &Path,
        embedder: E,
        embed_model: String,
        default_top_k: usize,
    ) -> Result<Library<E>> {
        let dim = probe_dimension(&embedder).await?;
        let store = ChunkStore::open(lancedb_dir, dim).await?;
        if store.count_rows().await? > 0 {
            let corpus = Corpus::open(&corpus_db)?;
            corpus.verify_index_stamps(&bookrack_ingest::current_index_stamps(
                &embed_model,
                dim as u32,
            ))?;
        }
        Ok(Library {
            store,
            embedder,
            corpus_db,
            catalog_db,
            default_top_k,
        })
    }

    /// The embedding dimension the vector store was opened at.
    pub fn dimension(&self) -> usize {
        self.store.dimension()
    }

    /// Search the library for passages matching `query`, nearest first.
    /// `top_k` falls back to the configured default when `None`.
    ///
    /// The async retrieval runs first, touching only the store and
    /// embedder; the corpus and catalog handles are opened only
    /// afterwards, for the synchronous citation step. Neither
    /// non-`Sync` handle is ever held across an await, so this future
    /// is `Send` and can serve requests on a multi-threaded runtime.
    pub async fn search(&self, query: &str, top_k: Option<usize>) -> Result<Vec<Citation>> {
        let top_k = top_k.unwrap_or(self.default_top_k);
        let hits = retrieve(query, &self.store, &self.embedder, top_k).await?;
        let corpus = Corpus::open(&self.corpus_db)?;
        let catalog = Catalog::open(&self.catalog_db)?;
        let citations = cite(&corpus, &catalog, hits)?;
        Ok(citations)
    }
}

/// Embed a fixed probe string to learn the model's output dimension.
async fn probe_dimension<E: Embedder>(embedder: &E) -> Result<usize> {
    let probe = embedder
        .embed_batch(&["dimension probe".to_string()])
        .await?;
    probe.first().map(Vec::len).ok_or(QueryError::EmptyProbe)
}
