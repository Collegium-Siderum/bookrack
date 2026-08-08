// SPDX-License-Identifier: Apache-2.0

//! bookrack-query: warm search state over the vector store.
//!
//! [`Library`] holds the embedder and the vector store open for the
//! process lifetime and answers cited-passage searches against them.
//! The `corpus` / `vectors` / `search` handles and their field shapes
//! stay behind it, so a caller reaches retrieval without naming the
//! database crates or their schema.
//!
//! [`dto`] carries the serialized shapes of the catalog-browse reads.
//! Those reads open the catalog read-only per call and need no embedder,
//! so they are implemented in `bookrack_ops::reads` — a catalog-only
//! `Ops` browses without paying for a dimension probe.

pub mod dto;

use std::path::{Path, PathBuf};

use bookrack_catalog::Catalog;
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_corpus::{Corpus, IndexStamps};
use bookrack_embed::Embedder;
use bookrack_normalize::NORMALIZE_VERSION;
use bookrack_search::{
    cite, embed_query, env_overrides, recall_with, retrieve_with, retrieve_with_partition,
};
use bookrack_vectors::ChunkStore;
pub use bookrack_vectors::SearchOptions;
use tokio::sync::RwLock;

// Re-exported so consumers name search results through this crate, not
// the crates behind it.
pub use bookrack_core::NodeId;
pub use bookrack_search::Citation;

/// Why a query operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// The embedder failed while embedding the dimension probe.
    #[error("embed error")]
    Embed(#[from] bookrack_embed::EmbedError),

    /// The vector store could not be opened or queried.
    #[error("vector store error")]
    Vectors(#[from] bookrack_vectors::VectorsError),

    /// A read-only corpus handle could not be opened.
    #[error("corpus error")]
    Corpus(#[from] bookrack_corpus::CorpusError),

    /// The catalog database could not be opened or queried.
    #[error("catalog error")]
    Catalog(#[from] bookrack_catalog::CatalogError),

    /// The underlying search operation failed.
    #[error("search error")]
    Search(#[from] bookrack_search::SearchError),

    /// The embedder returned no vector for the dimension probe.
    #[error("the embedder returned no vector for the dimension probe")]
    EmptyProbe,
}

impl bookrack_core::Explain for QueryError {
    fn explain(&self) -> bookrack_core::Problem {
        match self {
            // The embed leaf writes its own wording; the wrapper's own
            // `Display` ("embed error") names a module, not a failure.
            QueryError::Embed(e) => e.explain(),
            other => bookrack_core::Problem::from_error_chain(other),
        }
    }
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
    store: RwLock<Option<ChunkStore>>,
    embed_model: String,
    probed_dim: usize,
    embedder: E,
    corpus_db: PathBuf,
    catalog_db: PathBuf,
    lancedb_dir: PathBuf,
    default_top_k: usize,
    chunk_version: u32,
    /// Pipeline kind this library belongs to: `Book` for ingest,
    /// `Paper` for glean. Citations resolved through this library's
    /// search methods are tagged with this kind and use the matching
    /// breadcrumb shape.
    kind: ItemKind,
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
    ///
    /// `chunk_version` is the chunker-algorithm stamp the calling
    /// pipeline writes alongside its embeddings. Each pipeline (`ingest`
    /// for books, `glean` for papers) owns its own constant and threads
    /// it through here, so a single query crate can guard either index
    /// without taking a runtime dependency on either pipeline.
    pub async fn open(
        corpus_db: PathBuf,
        catalog_db: PathBuf,
        lancedb_dir: &Path,
        embedder: E,
        embed_model: String,
        default_top_k: usize,
        chunk_version: u32,
    ) -> Result<Library<E>> {
        let probed_dim = probe_dimension(&embedder).await?;
        let store = ChunkStore::try_open(lancedb_dir).await?;
        if let Some(s) = &store
            && s.count_rows().await? > 0
        {
            let corpus = Corpus::open_read_only(&corpus_db)?;
            corpus.verify_index_stamps(&current_stamps(
                &embed_model,
                probed_dim as u32,
                chunk_version,
            ))?;
        }
        Ok(Library {
            store: RwLock::new(store),
            embed_model,
            probed_dim,
            embedder,
            corpus_db,
            catalog_db,
            lancedb_dir: lancedb_dir.to_path_buf(),
            default_top_k,
            chunk_version,
            kind: ItemKind::Book,
        })
    }

    /// Tag this library with the pipeline kind it serves. Returns the
    /// library so the caller can chain off [`Library::open`]:
    ///
    /// ```ignore
    /// let papers_library = Library::open(...).await?.with_kind(ItemKind::Paper);
    /// ```
    pub fn with_kind(mut self, kind: ItemKind) -> Self {
        self.kind = kind;
        self
    }

    /// Drop the cached vector store and rebind it from the lance dir on
    /// disk. Called by [`bookrack_ops::LibraryHandle::ingest_book`] right
    /// after a successful ingest: the first ingest into a previously
    /// empty data dir creates the lance dir mid-process, and the
    /// `store=None` cached at [`Self::open`] would otherwise hide every
    /// subsequent search.
    ///
    /// Holds the write lock, so any in-flight searches finish before the
    /// rebind and any later search waits for it. Verifies the rebound
    /// store's stamps against the warm embedder for the same reason
    /// [`Self::open`] does.
    pub async fn refresh_store(&self) -> Result<()> {
        let fresh = ChunkStore::try_open(&self.lancedb_dir).await?;
        if let Some(s) = &fresh
            && s.count_rows().await? > 0
        {
            let corpus = Corpus::open_read_only(&self.corpus_db)?;
            corpus.verify_index_stamps(&current_stamps(
                &self.embed_model,
                self.probed_dim as u32,
                self.chunk_version,
            ))?;
        }
        let mut guard = self.store.write().await;
        *guard = fresh;
        Ok(())
    }

    /// Lazy-open the store under the read path. The daemon's [`Self::open`]
    /// records `store=None` when the data dir was empty at startup, so a
    /// later first ingest in the same process would leave the read path
    /// blind until [`Self::refresh_store`] runs. Search paths call this
    /// before reading so a misordered refresh — or a search that races a
    /// concurrent ingest — still observes the store.
    async fn ensure_store(&self) -> Result<()> {
        if self.store.read().await.is_some() {
            return Ok(());
        }
        let fresh = ChunkStore::try_open(&self.lancedb_dir).await?;
        let Some(s) = fresh else {
            return Ok(());
        };
        if s.count_rows().await? > 0 {
            let corpus = Corpus::open_read_only(&self.corpus_db)?;
            corpus.verify_index_stamps(&current_stamps(
                &self.embed_model,
                self.probed_dim as u32,
                self.chunk_version,
            ))?;
        }
        let mut guard = self.store.write().await;
        if guard.is_none() {
            *guard = Some(s);
        }
        Ok(())
    }

    /// The embedding dimension probed from the configured embedder.
    pub fn dimension(&self) -> usize {
        self.probed_dim
    }

    /// Borrow the warm embedder driving this library. Consumed by the
    /// registry-level ingest wrapper, which forwards the reference to
    /// [`bookrack_ingest::ingest_book`].
    pub fn embedder(&self) -> &E {
        &self.embedder
    }

    /// The `top_k` this library falls back to when a caller does not
    /// supply one. Sourced from `SearchConfig::top_k` at construction.
    pub fn default_top_k(&self) -> usize {
        self.default_top_k
    }

    /// The embedding model this library serves, as passed to
    /// [`Self::open`]. Two libraries reporting the same model embed a
    /// query into the same vector space, which is what lets a caller
    /// searching both of them embed once and recall twice through
    /// [`Self::search_with_vector`].
    pub fn embed_model(&self) -> &str {
        &self.embed_model
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
        self.search_with(query, env_overrides(), top_k).await
    }

    /// Variant of [`Self::search`] that layers per-call overrides on top
    /// of the persisted meta defaults — see
    /// [`bookrack_search::retrieve_with`] for the merge order. Callers
    /// that already source overrides themselves (CLI flags, MCP tool
    /// arguments) pass them through here instead of reading the
    /// `BOOKRACK_VECTORS_*` env vars again.
    pub async fn search_with(
        &self,
        query: &str,
        overrides: SearchOptions,
        top_k: Option<usize>,
    ) -> Result<Vec<Citation>> {
        self.ensure_store().await?;
        let guard = self.store.read().await;
        let Some(store) = guard.as_ref() else {
            return Ok(Vec::new());
        };
        let top_k = top_k.unwrap_or(self.default_top_k);
        let hits = retrieve_with(
            query,
            store,
            &self.embedder,
            &self.lancedb_dir,
            overrides,
            top_k,
        )
        .await?;
        let corpus = Corpus::open_read_only(&self.corpus_db)?;
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        let citations = cite(&corpus, &catalog, hits, self.kind)?;
        Ok(citations)
    }

    /// Embed `query` with this library's warm embedder, producing the
    /// vector [`Self::search_with_vector`] recalls with.
    ///
    /// Split out of [`Self::search_with`] so a caller searching several
    /// libraries that report the same [`Self::embed_model`] pays one
    /// embed round trip for the whole fan-out.
    pub async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        Ok(embed_query(query, &self.embedder).await?)
    }

    /// Variant of [`Self::search_with`] that recalls with a query vector
    /// the caller already holds, skipping this library's embedder.
    ///
    /// `query_vector` must come from an embedder serving this library's
    /// [`Self::embed_model`] — the store checks only the width, so a
    /// vector embedded under a different model of equal width recalls
    /// silently wrong passages. Everything after recall is identical to
    /// [`Self::search_with`]: the same override merge, the same
    /// citation step, the same `kind` stamped on each hit.
    pub async fn search_with_vector(
        &self,
        query_vector: &[f32],
        overrides: SearchOptions,
        top_k: Option<usize>,
    ) -> Result<Vec<Citation>> {
        self.ensure_store().await?;
        let guard = self.store.read().await;
        let Some(store) = guard.as_ref() else {
            return Ok(Vec::new());
        };
        let top_k = top_k.unwrap_or(self.default_top_k);
        let hits = recall_with(query_vector, store, &self.lancedb_dir, overrides, top_k).await?;
        let corpus = Corpus::open_read_only(&self.corpus_db)?;
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        let citations = cite(&corpus, &catalog, hits, self.kind)?;
        Ok(citations)
    }

    /// Search inside one book's id partition, nearest first. Equivalent
    /// to [`Self::search`] but with a `start_node_id BETWEEN ...`
    /// predicate that confines recall to the chunks owned by
    /// `intake_id`. An unknown intake or one with no chunks returns an
    /// empty `Vec`.
    pub async fn search_in_book(
        &self,
        intake_id: i64,
        query: &str,
        top_k: Option<usize>,
    ) -> Result<Vec<Citation>> {
        self.search_in_book_with(intake_id, query, env_overrides(), top_k)
            .await
    }

    /// Variant of [`Self::search_in_book`] that layers per-call overrides
    /// on top of the persisted meta defaults — see
    /// [`bookrack_search::retrieve_with_partition`] for the merge order.
    pub async fn search_in_book_with(
        &self,
        intake_id: i64,
        query: &str,
        overrides: SearchOptions,
        top_k: Option<usize>,
    ) -> Result<Vec<Citation>> {
        self.ensure_store().await?;
        let guard = self.store.read().await;
        let Some(store) = guard.as_ref() else {
            return Ok(Vec::new());
        };
        let top_k = top_k.unwrap_or(self.default_top_k);
        let partition = PartitionIdx::new(intake_id);
        let hits = retrieve_with_partition(
            query,
            store,
            &self.embedder,
            &self.lancedb_dir,
            overrides,
            top_k,
            partition,
        )
        .await?;
        let corpus = Corpus::open_read_only(&self.corpus_db)?;
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        let citations = cite(&corpus, &catalog, hits, self.kind)?;
        Ok(citations)
    }

    /// Search inside one paper's id partition, nearest first. Equivalent
    /// to [`Self::search_in_book`] but cites with the paper-side
    /// breadcrumb (container title + paper title) instead of the
    /// book-side chapter/section trail.
    pub async fn search_in_paper(
        &self,
        intake_id: i64,
        query: &str,
        top_k: Option<usize>,
    ) -> Result<Vec<Citation>> {
        self.search_in_paper_with(intake_id, query, env_overrides(), top_k)
            .await
    }

    /// Variant of [`Self::search_in_paper`] that layers per-call
    /// overrides on top of the persisted meta defaults.
    pub async fn search_in_paper_with(
        &self,
        intake_id: i64,
        query: &str,
        overrides: SearchOptions,
        top_k: Option<usize>,
    ) -> Result<Vec<Citation>> {
        self.ensure_store().await?;
        let guard = self.store.read().await;
        let Some(store) = guard.as_ref() else {
            return Ok(Vec::new());
        };
        let top_k = top_k.unwrap_or(self.default_top_k);
        let partition = PartitionIdx::new(intake_id);
        let hits = retrieve_with_partition(
            query,
            store,
            &self.embedder,
            &self.lancedb_dir,
            overrides,
            top_k,
            partition,
        )
        .await?;
        let corpus = Corpus::open_read_only(&self.corpus_db)?;
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        let citations = cite(&corpus, &catalog, hits, self.kind)?;
        Ok(citations)
    }
}

/// Embed a fixed probe string to learn the model's output dimension.
/// The text is [`bookrack_embed::DIMENSION_PROBE_TEXT`], so a probe
/// against a `(model, url)` pair the process already embedded with is
/// answered from the embed crate's dimension cache without a network
/// round-trip.
async fn probe_dimension<E: Embedder>(embedder: &E) -> Result<usize> {
    let probe = embedder
        .embed_batch(&[bookrack_embed::DIMENSION_PROBE_TEXT.to_string()])
        .await?;
    probe.first().map(Vec::len).ok_or(QueryError::EmptyProbe)
}

/// Assemble the [`IndexStamps`] this serve binary expects, given the
/// embed model and dimension known to the warm library and the
/// chunk-algorithm stamp the calling pipeline owns. `normalize_version`
/// comes from the workspace `bookrack-normalize` constant, since both
/// pipelines share it.
fn current_stamps(embed_model: &str, vector_dim: u32, chunk_version: u32) -> IndexStamps {
    IndexStamps {
        embed_model: embed_model.to_string(),
        vector_dim,
        chunk_version,
        normalize_version: NORMALIZE_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn wrapper_variant_display_does_not_embed_source_message() {
        let inner = bookrack_embed::EmbedError::Unreachable("boom".to_string());
        let outer = QueryError::Embed(inner);

        assert_eq!(outer.to_string(), "embed error");
        let src = outer.source().expect("source set by #[from]");
        assert_eq!(src.to_string(), "Ollama unreachable: boom");
    }
}
