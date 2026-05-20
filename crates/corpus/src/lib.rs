// SPDX-License-Identifier: Apache-2.0

//! `corpus.db` — the node tree and the node-id partition allocator.
//!
//! This crate owns the rebuildable half of the data model: the corpus
//! node tree (organizing nodes and leaves), the body text those leaves
//! carry, and the allocator that hands every book a private block of
//! the global node-id space.
//!
//! The store is *rebuildable by design* — it can be reconstructed from
//! the source files plus `catalog.db` — so it carries no source of
//! truth. Identity and curated metadata live in `catalog.db`, which
//! references nodes here only by bare integer `node_id` (a soft
//! reference, no cross-database foreign key).
//!
//! All SQL is contained in this crate. Callers work through the typed
//! [`Corpus`] handle and never assemble queries themselves; this is the
//! crate boundary that keeps the two-database split honest.

mod db;
mod node;
mod partition;

pub use bookrack_core::{NodeId, NodeType, PartitionIdx};
pub use db::{Corpus, SCHEMA_VERSION};
pub use node::{NewNode, Node};
pub use partition::Partition;

/// A fallible `corpus` operation.
pub type Result<T> = std::result::Result<T, CorpusError>;

/// Why a `corpus` operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CorpusError {
    /// The underlying SQLite layer reported an error.
    #[error("corpus database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database was built by a different schema revision than this
    /// binary understands. The schema carries no migrations yet, so the
    /// only resolution is to rebuild the corpus. `found` is reported as
    /// the raw stored string, since a foreign database may hold a value
    /// that is not even a version number.
    #[error("corpus schema mismatch: database reports {found:?}, this build expects v{expected}")]
    SchemaMismatch {
        /// Schema version string recorded in the opened database.
        found: String,
        /// Schema version this binary was compiled against.
        expected: u32,
    },

    /// An intake already owns a partition. Partition allocation happens
    /// exactly once per intake; a book is re-ingested by removing it
    /// first, not by allocating a second partition.
    #[error("intake {0} already owns a node-id partition")]
    PartitionAlreadyAllocated(i64),

    /// An operation named a partition that does not exist — typically
    /// allocating node ids before the partition itself was allocated.
    #[error("partition {0} does not exist")]
    UnknownPartition(PartitionIdx),

    /// A partition cannot fit the requested number of further nodes.
    /// Each book is capped at [`bookrack_core::NODE_CAPACITY`] nodes.
    #[error("partition {partition} cannot allocate {requested} more node ids")]
    PartitionExhausted {
        /// The partition that ran out of room.
        partition: PartitionIdx,
        /// How many ids the exhausted request asked for.
        requested: u32,
    },

    /// A node violates a structural invariant of the tree. Rejected at
    /// the write boundary so a malformed node never reaches the store.
    #[error("invalid node {node_id}: {reason}")]
    InvalidNode {
        /// The offending node's id.
        node_id: i64,
        /// What rule it broke.
        reason: &'static str,
    },
}
