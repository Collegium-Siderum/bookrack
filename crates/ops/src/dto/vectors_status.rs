// SPDX-License-Identifier: Apache-2.0

//! Snapshot of the vector store: chunk table size, every ANN index
//! LanceDB enumerates, the persisted ANN config, and any drift between
//! the on-disk meta and the indices LanceDB actually carries.
//!
//! The DTO is what [`crate::reads::vectors::status`] returns. Renderers
//! sit in the calling crate — the operator-facing text view in the CLI,
//! the JSON wire shape in the MCP `library.vectors_status` tool — and
//! never recompute the values that live here.

use serde::Serialize;

/// One ANN index entry under [`VectorsStatus::indices`].
#[derive(Debug, Clone, Serialize)]
pub struct VectorsIndexStatus {
    /// LanceDB's name for the index.
    pub name: String,
    /// Per-index statistics, when LanceDB returned a non-empty record.
    pub stats: Option<VectorsIndexStats>,
}

/// Mirror of the fields the status surface renders from
/// `lancedb::index::IndexStatistics`. Kept here so the wire shape is
/// not pinned to a transitive crate's serde behaviour.
#[derive(Debug, Clone, Serialize)]
pub struct VectorsIndexStats {
    /// Index family ("IvfFlat" / "IvfPq" / ...). Rendered with
    /// `Debug` against `lancedb::index::IndexType`.
    pub index_type: String,
    /// Rows covered by this index.
    pub num_indexed_rows: usize,
    /// Rows added since the last build.
    pub num_unindexed_rows: usize,
    /// Number of shards (partitions) this index is split into.
    pub num_indices: Option<u32>,
    /// Training loss the index recorded at build time.
    pub loss: Option<f64>,
}

/// Subset of `bookrack_vectors::AnnConfig` the status surface renders.
#[derive(Debug, Clone, Serialize)]
pub struct VectorsAnnConfig {
    /// ANN kind label (`"IvfFlat"` / `"IvfPq"` / `"BruteForce"` / ...).
    pub kind: String,
    pub num_partitions: u32,
    pub nprobes: u32,
    pub refine_factor: Option<u32>,
}

/// Subset of `bookrack_vectors::VectorsMeta` the status surface renders.
#[derive(Debug, Clone, Serialize)]
pub struct VectorsMetaSummary {
    /// Recorded ANN kind.
    pub kind: String,
    /// LanceDB index name the build wrote.
    pub lance_index_name: String,
    /// Rows added since the last rebuild.
    pub churn_since_rebuild: u64,
}

/// Drift between the persisted meta and what LanceDB enumerates.
#[derive(Debug, Clone, Serialize)]
pub struct VectorsMetaDrift {
    /// Index name the meta claims should exist.
    pub expected_index: String,
    /// Index names LanceDB actually returned.
    pub found_indices: Vec<String>,
}

/// Snapshot of the vector store: chunk table size, every ANN index
/// LanceDB enumerates, the persisted ANN config, and any drift between
/// the on-disk meta and the indices LanceDB actually carries.
#[derive(Debug, Clone, Serialize)]
pub struct VectorsStatus {
    /// Number of rows currently in the chunk table. `None` when the
    /// corpus carries no `vector_dim` stamp — i.e. no chunks have ever
    /// been ingested into this library.
    pub row_count: Option<usize>,
    /// Distinct ANN indices LanceDB reports for the table, in the
    /// order returned. Empty means brute-force search.
    pub indices: Vec<VectorsIndexStatus>,
    /// Persisted ANN config, when present in the vectors_meta record.
    pub ann_config: Option<VectorsAnnConfig>,
    /// Lightweight summary of the persisted vectors_meta record.
    pub meta: Option<VectorsMetaSummary>,
    /// Set when the persisted meta claims an index name LanceDB does
    /// not actually carry — visible after-effect of a failed rebuild or
    /// of a manual intervention on the lancedb directory.
    pub meta_drift: Option<VectorsMetaDrift>,
}
