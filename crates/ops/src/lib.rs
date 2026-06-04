// SPDX-License-Identifier: Apache-2.0

//! bookrack-ops: the shared operation layer behind CLI and MCP.
//!
//! Each user-visible action — searching, listing books, editing metadata
//! — is one function here. The CLI parses arguments with `clap` and calls
//! into this crate; the MCP server deserializes tool arguments and calls
//! into this crate. Output shapes are DTOs that both surfaces serialize.
//!
//! This phase lays the scaffolding: the [`Ops`] handle wraps one warm
//! [`bookrack_query::Library`] together with the file-system paths every
//! op needs, the read modules proxy straight through to the library
//! facade, and the write module is reserved for later phases.

pub mod dto;
pub mod reads;
pub mod writes;

use std::path::{Path, PathBuf};

use bookrack_catalog::ActorKind;
use bookrack_embed::Embedder;
use bookrack_query::{Library, QueryError};

pub use bookrack_query::Citation;
pub use dto::audit::Caller;

/// Why an op failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpsError {
    /// The underlying read facade returned an error.
    #[error("query error: {0}")]
    Query(#[from] QueryError),

    /// A catalog read or write failed.
    #[error("catalog error: {0}")]
    Catalog(#[from] bookrack_catalog::CatalogError),

    /// A corpus read failed.
    #[error("corpus error: {0}")]
    Corpus(#[from] bookrack_corpus::CorpusError),

    /// The named intake does not exist.
    #[error("no intake registered for id {intake_id}")]
    IntakeNotFound {
        /// The intake id the caller asked for.
        intake_id: i64,
    },
}

/// A fallible op.
pub type Result<T> = std::result::Result<T, OpsError>;

/// Warm, shareable op state.
///
/// Holds the warm [`Library`] together with the file-system paths every
/// op needs and the [`Caller`] every write op stamps onto its audit
/// row. Reads proxy through the library; writes (added in a later
/// phase) open the catalog read-write per call.
pub struct Ops<E: Embedder> {
    library: Library<E>,
    // The path fields below stay populated even when no write op is in
    // scope yet, so a later phase can open the catalog read-write or the
    // corpus directly without changing the constructor signature.
    #[allow(dead_code)]
    corpus_db: PathBuf,
    #[allow(dead_code)]
    catalog_db: PathBuf,
    #[allow(dead_code)]
    lancedb_dir: PathBuf,
    caller: Caller,
}

impl<E: Embedder> Ops<E> {
    /// Wrap a warm [`Library`] in an [`Ops`] for the given caller.
    pub fn new(
        library: Library<E>,
        corpus_db: PathBuf,
        catalog_db: PathBuf,
        lancedb_dir: &Path,
        caller: Caller,
    ) -> Ops<E> {
        Ops {
            library,
            corpus_db,
            catalog_db,
            lancedb_dir: lancedb_dir.to_path_buf(),
            caller,
        }
    }

    /// The caller identity stamped on every write this `Ops` records.
    pub fn caller(&self) -> &Caller {
        &self.caller
    }

    /// The embedding dimension the vector store was opened at.
    pub fn dimension(&self) -> usize {
        self.library.dimension()
    }

    /// Borrow the underlying read facade. Read ops on `Ops` proxy to it.
    pub(crate) fn library(&self) -> &Library<E> {
        &self.library
    }
}

/// Conventional `actor_detail` value for the CLI surface.
pub const ACTOR_DETAIL_CLI: &str = "cli";

/// Conventional `actor_detail` value for the MCP surface.
pub const ACTOR_DETAIL_MCP: &str = "mcp";

impl Caller {
    /// A CLI caller: [`ActorKind::Human`] with `actor_detail = "cli"`.
    pub fn cli() -> Caller {
        Caller {
            actor_kind: ActorKind::Human,
            actor_detail: Some(ACTOR_DETAIL_CLI.to_string()),
            session_id: None,
            reason: None,
        }
    }

    /// An MCP caller: [`ActorKind::Llm`] with `actor_detail = "mcp"`.
    pub fn mcp() -> Caller {
        Caller {
            actor_kind: ActorKind::Llm,
            actor_detail: Some(ACTOR_DETAIL_MCP.to_string()),
            session_id: None,
            reason: None,
        }
    }
}
