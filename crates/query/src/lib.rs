// SPDX-License-Identifier: Apache-2.0

//! bookrack-query: the read-only query facade.
//!
//! A single capability surface over the corpus and vector store that
//! query consumers — the MCP server, the CLI — call without touching the
//! database crates or their schema. Consumers depend only on this crate;
//! the `corpus` / `vectors` / `search` handles and their field shapes stay
//! behind it. Adding or removing a capability is adding or removing one
//! method here.

pub mod dto;

use std::path::{Path, PathBuf};

use bookrack_catalog::{BOOK_SCOPE, Catalog, IntakeFilter, IntakeStatus};
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_search::{cite, env_overrides, retrieve_with, retrieve_with_partition};
use bookrack_vectors::ChunkStore;

use crate::dto::{
    BookDetail, BookFilter, BookSummary, LibraryStats, ListBooksResult, MAX_TOC_NODES, Toc,
    TocNode, clamp_limit,
};

// Re-exported so consumers name query results through this crate, not the
// crates behind the facade.
pub use bookrack_catalog::{STATUS_ACKNOWLEDGED, STATUS_APPROVED, STATUS_PENDING, STATUS_REJECTED};
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
    lancedb_dir: PathBuf,
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
            lancedb_dir: lancedb_dir.to_path_buf(),
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
        let hits = retrieve_with(
            query,
            &self.store,
            &self.embedder,
            &self.lancedb_dir,
            env_overrides(),
            top_k,
        )
        .await?;
        let corpus = Corpus::open(&self.corpus_db)?;
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        let citations = cite(&corpus, &catalog, hits)?;
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
        let top_k = top_k.unwrap_or(self.default_top_k);
        let partition = PartitionIdx::new(intake_id);
        let hits = retrieve_with_partition(
            query,
            &self.store,
            &self.embedder,
            &self.lancedb_dir,
            env_overrides(),
            top_k,
            partition,
        )
        .await?;
        let corpus = Corpus::open(&self.corpus_db)?;
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        let citations = cite(&corpus, &catalog, hits)?;
        Ok(citations)
    }

    /// Aggregate counts across the catalog: intakes by status / format,
    /// book states by stage, retrieval issues by status. Drives the
    /// `library.stats` MCP tool.
    pub fn stats(&self) -> Result<LibraryStats> {
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        let mut intake_counts_by_status = std::collections::BTreeMap::new();
        for status in IntakeStatus::ALL {
            let n = catalog.count_intakes_by_status(std::slice::from_ref(&status))?;
            intake_counts_by_status.insert(status.as_str().to_string(), n);
        }
        let mut intake_count_by_format = std::collections::BTreeMap::new();
        for format in ["epub", "pdf", "mobi", "azw3", "txt"] {
            let n = catalog.count_intakes_by_format(format)?;
            if n > 0 {
                intake_count_by_format.insert(format.to_string(), n);
            }
        }
        let mut book_state_counts_by_stage = std::collections::BTreeMap::new();
        for stage in [
            "extract",
            "structure",
            "metadata",
            "chunk",
            "embed",
            "ready",
        ] {
            let n = catalog.count_book_states_by_stage(stage)?;
            if n > 0 {
                book_state_counts_by_stage.insert(stage.to_string(), n);
            }
        }
        let mut retrieval_issue_counts_by_status = std::collections::BTreeMap::new();
        for status in ["open", "triaged", "resolved", "wontfix"] {
            let n = catalog.count_retrieval_issues_by_status(&[status])?;
            if n > 0 {
                retrieval_issue_counts_by_status.insert(status.to_string(), n);
            }
        }
        Ok(LibraryStats {
            intake_counts_by_status,
            intake_count_by_format,
            book_state_counts_by_stage,
            retrieval_issue_counts_by_status,
        })
    }

    /// List books in catalog order, paginated. Equivalent to
    /// [`Self::find_books`] with an empty filter.
    pub fn list_books(&self, limit: u32, offset: u32) -> Result<ListBooksResult> {
        self.find_books(BookFilter::default(), limit, offset)
    }

    /// List books matching `filter`, paginated. The limit is clamped to
    /// [`dto::MAX_LIST_LIMIT`]; `truncated` is set when the clamp took
    /// effect or when `total > offset + books.len()`.
    pub fn find_books(
        &self,
        filter: BookFilter,
        limit: u32,
        offset: u32,
    ) -> Result<ListBooksResult> {
        let (effective_limit, clamp_triggered) = clamp_limit(limit);
        let catalog = Catalog::open_read_only(&self.catalog_db)?;

        let catalog_filter = IntakeFilter {
            title_substring: filter.title_substring.as_deref(),
            contributor_name: filter.contributor_name.as_deref(),
            contributor_role: filter.contributor_role.as_deref(),
            statuses: filter.statuses.as_slice(),
            format: filter.format.as_deref(),
            ..IntakeFilter::default()
        };
        let intakes = catalog.find_intakes(&catalog_filter, effective_limit, offset)?;
        let total = catalog.count_find_intakes(&catalog_filter)?;

        let mut books = Vec::with_capacity(intakes.len());
        for intake in intakes {
            let effective = catalog.effective_publication_attrs(intake.intake_id, BOOK_SCOPE)?;
            let title = effective.get("title").map(str::to_string);
            let contributors = catalog.contributors_for_address(intake.intake_id, BOOK_SCOPE)?;
            let top_contributor = contributors.first().map(|c| c.name.clone());
            books.push(BookSummary::from_intake(&intake, title, top_contributor));
        }
        let returned = books.len() as u64;
        let truncated = clamp_triggered || u64::from(offset) + returned < total;
        Ok(ListBooksResult {
            books,
            total,
            truncated,
        })
    }

    /// Fetch the full bibliographic record of one book by intake id, or
    /// `None` if no such book is registered.
    pub fn show_book(&self, intake_id: i64) -> Result<Option<BookDetail>> {
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        let Some(intake) = catalog.intake_by_id(intake_id)? else {
            return Ok(None);
        };
        let effective = catalog.effective_publication_attrs(intake.intake_id, BOOK_SCOPE)?;
        let contributors = catalog.contributors_for_address(intake.intake_id, BOOK_SCOPE)?;
        Ok(Some(BookDetail::build(intake, effective, contributors)))
    }

    /// Project the table of contents of one book — the organizing
    /// nodes under the book root, in depth-first TOC order. Returns
    /// `None` when no book root exists for `intake_id`.
    pub fn show_toc(&self, intake_id: i64) -> Result<Option<Toc>> {
        let catalog = Catalog::open_read_only(&self.catalog_db)?;
        if catalog.intake_by_id(intake_id)?.is_none() {
            return Ok(None);
        }
        let corpus = Corpus::open(&self.corpus_db)?;
        let book_root_id = PartitionIdx::new(intake_id).root();
        let nodes = corpus.toc_for_book(book_root_id, MAX_TOC_NODES + 1)?;
        if nodes.is_empty() {
            return Ok(None);
        }
        let truncated = nodes.len() > MAX_TOC_NODES;
        let projected: Vec<TocNode> = nodes
            .iter()
            .take(MAX_TOC_NODES)
            .map(TocNode::from_node)
            .collect();
        Ok(Some(Toc {
            intake_id,
            nodes: projected,
            truncated,
        }))
    }
}

/// Embed a fixed probe string to learn the model's output dimension.
async fn probe_dimension<E: Embedder>(embedder: &E) -> Result<usize> {
    let probe = embedder
        .embed_batch(&["dimension probe".to_string()])
        .await?;
    probe.first().map(Vec::len).ok_or(QueryError::EmptyProbe)
}
