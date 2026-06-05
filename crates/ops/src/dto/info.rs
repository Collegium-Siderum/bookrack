// SPDX-License-Identifier: Apache-2.0

//! The library-info status card.
//!
//! [`LibraryInfo`] is what `library.info` returns and what
//! `bookrack info` renders: schema versions, embedder configuration,
//! stamped index parameters, vector-store state, and rough disk usage.

use serde::Serialize;

use bookrack_vectors::VectorsMeta;

/// One-page library status: which library is open, how it's configured,
/// what the stamps say, and how big it is on disk.
#[derive(Debug, Clone, Serialize)]
pub struct LibraryInfo {
    /// Where the library lives on disk.
    pub data_dir: String,
    /// Registry name of the open library, when one was selected.
    pub library_name: Option<String>,
    /// How the data-dir was resolved (database flag / env / registry
    /// default / ...).
    pub resolution_source: String,
    /// Ollama HTTP endpoint the daemon will reach for embeddings.
    pub ollama_url: String,
    /// Model tag the daemon is configured to embed with.
    pub embed_model_configured: String,
    /// `corpus.db` schema version the binary expects.
    pub corpus_schema_version_expected: u32,
    /// `catalog.db` schema version the binary expects.
    pub catalog_schema_version_expected: u32,
    /// `catalog.db` schema version stamped in `catalog_meta`, or `None`
    /// if no row has been written yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_schema_version_on_disk: Option<String>,
    /// Stamps the corpus carries about the index it was built with.
    pub corpus_stamps: CorpusStamps,
    /// Persisted vector-store metadata (ANN configuration, snapshot at
    /// build time); absent on a library that has never been indexed.
    pub vectors_meta: Option<VectorsMeta>,
    /// Live row count of the vector store, when readable.
    pub current_chunks: Option<usize>,
    /// Total intake rows recorded in the catalog, when readable.
    pub intake_count: Option<u64>,
    /// Books that have reached the `ready` lifecycle stage, when
    /// readable.
    pub ready_book_count: Option<u64>,
    /// Rough byte sizes of the three on-disk stores.
    pub disk: DiskUsage,
}

/// Build-time stamps the corpus tracks about its index.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CorpusStamps {
    /// Embedding model the chunks were embedded with, when stamped.
    pub embed_model: Option<String>,
    /// Vector dimension the store was built at, when stamped.
    pub vector_dim: Option<String>,
    /// Chunk-version stamp.
    pub chunk_version: Option<String>,
    /// Normalize-version stamp.
    pub normalize_version: Option<String>,
    /// Schema version the corpus is at on disk, when readable.
    pub schema_version_on_disk: Option<String>,
}

/// Rough disk usage of the library's three stores.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DiskUsage {
    /// `catalog.db` size, when readable.
    pub catalog_db: Option<u64>,
    /// `corpus.db` size, when readable.
    pub corpus_db: Option<u64>,
    /// Total size of the LanceDB directory, when readable.
    pub lancedb_dir: Option<u64>,
}
