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
mod index_meta;
mod node;
mod partition;
mod resolve;

pub use bookrack_core::{NodeId, NodeType, PartitionIdx, Scope};
pub use db::{Corpus, SCHEMA_VERSION};
pub use index_meta::{
    CHUNK_VERSION_KEY, EMBED_MODEL_KEY, IndexStamps, NORMALIZE_VERSION_KEY, VECTOR_DIM_KEY,
};
pub use node::{NewNode, Node};
pub use partition::Partition;
pub use resolve::ResolveError;

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

    /// The database carries a `min_reader_version` stamp this binary
    /// cannot meet. The writer required a reader at version `required`
    /// or higher; this build is at `current`. Opening fails so the
    /// operator can run a newer build rather than serve stale data.
    #[error(
        "corpus requires a newer reader: stamp demands v{required}, \
         this build is at v{current}"
    )]
    ReaderTooOld {
        /// The `min_reader_version` value recorded on disk.
        required: u32,
        /// [`bookrack_dbkit::READER_VERSION`] this build was compiled at.
        current: u32,
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

    /// A recorded index-build stamp differs from this binary's
    /// expectation — a different embedding model or a bumped algorithm
    /// version. The store is rebuildable, so the resolution is to rebuild
    /// it. `found` is reported as the raw stored string, since a foreign
    /// index may hold a value that is not even well-formed.
    #[error(
        "index stamp mismatch on {key}: database reports {found:?}, this build expects {expected:?}"
    )]
    IndexStampMismatch {
        /// The `index_meta` key whose value drifted.
        key: &'static str,
        /// The value recorded in the opened index.
        found: String,
        /// The value this binary was compiled or configured with.
        expected: String,
    },

    /// An existing, non-empty index carries no build stamps, so it
    /// predates version stamping and cannot be served safely. Rebuild the
    /// corpus.
    #[error("index carries no build stamps; rebuild the corpus")]
    IndexNotStamped,

    /// The on-disk schema disagrees with the compiled-in specs. Surfaces
    /// from the read-only open path so a drifted file is refused at open
    /// rather than discovered mid-query.
    #[error("corpus schema verification failed: {0}")]
    Verify(#[from] bookrack_dbkit::VerifyError),
}
