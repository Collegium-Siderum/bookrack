// SPDX-License-Identifier: Apache-2.0

//! bookrack-ops: the shared operation layer behind CLI and MCP.
//!
//! Each user-visible action — searching, listing books, editing metadata
//! — is one function here. The CLI parses arguments with `clap` and calls
//! into this crate; the MCP server deserializes tool arguments and calls
//! into this crate. Output shapes are DTOs that both surfaces serialize.
//!
//! Two constructors:
//!
//! - [`Ops::with_library`] holds a warm [`bookrack_query::Library`], so
//!   search ops are available. The MCP daemon and CLI subcommands that
//!   need vector recall (`bookrack query`) use this path.
//! - [`Ops::catalog_only`] omits the embedder and vector store, so a
//!   short-lived CLI process that only browses the catalog does not pay
//!   the Ollama probe cost. Search ops on this variant fail with
//!   [`OpsError::SearchUnavailable`].
//!
//! Reads open the catalog read-only per call; writes (added in a later
//! phase) open it read-write per call and record a
//! [`bookrack_catalog::MetadataAudit`] row tagged with the [`Caller`]
//! this [`Ops`] was built with.

pub mod dto;
pub mod reads;
pub(crate) mod recorder;
pub mod registry;
pub mod writes;

use std::path::{Path, PathBuf};

use bookrack_catalog::ActorKind;
use bookrack_embed::Embedder;
use bookrack_query::{Library, QueryError};

pub use bookrack_query::{Citation, SearchOptions};
pub use dto::audit::Caller;
pub use recorder::with_caller_override;

/// Why an op failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpsError {
    /// The underlying read facade returned an error.
    #[error("query error")]
    Query(#[from] QueryError),

    /// A catalog read or write failed.
    #[error("catalog error")]
    Catalog(#[from] bookrack_catalog::CatalogError),

    /// A corpus read failed.
    #[error("corpus error")]
    Corpus(#[from] bookrack_corpus::CorpusError),

    /// The vector store layer reported an error.
    #[error("vectors error")]
    Vectors(#[from] bookrack_vectors::VectorsError),

    /// The named intake does not exist.
    #[error("no intake registered for id {intake_id}")]
    IntakeNotFound {
        /// The intake id the caller asked for.
        intake_id: i64,
    },

    /// The named field is not a curator-editable bibliographic
    /// attribute. The message carries the full editable set so the
    /// caller can self-correct without a second lookup.
    #[error(
        "unknown metadata field {field:?}; editable fields are: {}",
        bookrack_catalog::EDITABLE_FIELDS.join(", ")
    )]
    UnknownMetadataField {
        /// The field name the caller asked to edit.
        field: String,
    },

    /// The named corpus node does not exist.
    #[error("no corpus node exists with id {node_id}")]
    NodeNotFound {
        /// The node id the caller asked for.
        node_id: i64,
    },

    /// A context read was anchored on a node that is not a content
    /// leaf. Organizing nodes are read with `read_span` instead.
    #[error("node {node_id} is not a content leaf; read organizing nodes with read_span")]
    NotALeaf {
        /// The node id the caller asked for.
        node_id: i64,
    },

    /// A span read targeted a node that is not an organizing node.
    /// Content leaves are read with `read_context` instead.
    #[error("node {node_id} is not an organizing node; read leaves with read_context")]
    NotOrganizing {
        /// The node id the caller asked for.
        node_id: i64,
    },

    /// A search op was issued on an [`Ops`] built without a vector store.
    #[error("search is not available on a catalog-only Ops handle")]
    SearchUnavailable,

    /// Catch-all for ad-hoc errors that have no dedicated variant.
    #[error(transparent)]
    Other(anyhow::Error),
}

/// A fallible op.
pub type Result<T> = std::result::Result<T, OpsError>;

/// Warm, shareable op state.
///
/// Holds the file-system paths every op needs and, optionally, a warm
/// [`Library`] for search. The path set covers both reads (catalog,
/// corpus, vector store) and the registry-mediated ingest write path
/// (books staging directory, catalog backup directory).
pub struct Ops<E: Embedder> {
    library: Option<Library<E>>,
    corpus_db: PathBuf,
    catalog_db: PathBuf,
    lancedb_dir: PathBuf,
    books_dir: PathBuf,
    backup_dir: PathBuf,
    caller: Caller,
}

impl<E: Embedder> Ops<E> {
    /// Build an `Ops` over a warm [`Library`]. Use this when search ops
    /// are needed.
    pub fn with_library(
        library: Library<E>,
        corpus_db: PathBuf,
        catalog_db: PathBuf,
        lancedb_dir: &Path,
        books_dir: PathBuf,
        backup_dir: PathBuf,
        caller: Caller,
    ) -> Ops<E> {
        Ops {
            library: Some(library),
            corpus_db,
            catalog_db,
            lancedb_dir: lancedb_dir.to_path_buf(),
            books_dir,
            backup_dir,
            caller,
        }
    }

    /// Build an `Ops` over the catalog and corpus only. Search ops on
    /// this handle return [`OpsError::SearchUnavailable`]; reads of book
    /// metadata, TOC, stats, and audit trails all work. This skips the
    /// embedder probe, which a short-lived CLI invocation cannot afford
    /// to pay on every call.
    pub fn catalog_only(
        corpus_db: PathBuf,
        catalog_db: PathBuf,
        lancedb_dir: &Path,
        books_dir: PathBuf,
        backup_dir: PathBuf,
        caller: Caller,
    ) -> Ops<E> {
        Ops {
            library: None,
            corpus_db,
            catalog_db,
            lancedb_dir: lancedb_dir.to_path_buf(),
            books_dir,
            backup_dir,
            caller,
        }
    }

    /// The caller identity this `Ops` was built with.
    pub fn caller(&self) -> &Caller {
        &self.caller
    }

    /// The caller identity to stamp on rows recorded by the current
    /// call: the task-scope override installed by
    /// [`with_caller_override`] when one is active (e.g. for tool calls
    /// arriving over MCP on a shared `Ops`), otherwise the caller this
    /// `Ops` was built with.
    pub fn effective_caller(&self) -> Caller {
        recorder::caller_override().unwrap_or_else(|| self.caller.clone())
    }

    /// The embedding dimension the vector store was opened at, if this
    /// `Ops` was built with a warm library.
    pub fn dimension(&self) -> Option<usize> {
        self.library.as_ref().map(Library::dimension)
    }

    /// Borrow the underlying read facade, or [`None`] when this `Ops`
    /// was built catalog-only.
    pub(crate) fn library(&self) -> Option<&Library<E>> {
        self.library.as_ref()
    }

    pub(crate) fn corpus_db(&self) -> &Path {
        &self.corpus_db
    }

    pub(crate) fn catalog_db(&self) -> &Path {
        &self.catalog_db
    }

    pub(crate) fn lancedb_dir(&self) -> &Path {
        &self.lancedb_dir
    }

    pub(crate) fn books_dir(&self) -> &Path {
        &self.books_dir
    }

    pub(crate) fn backup_dir(&self) -> &Path {
        &self.backup_dir
    }

    /// Borrow the warm embedder, if this `Ops` was built with a library.
    /// Used by the registry-level ingest wrapper, which feeds the
    /// embedder into [`bookrack_ingest::ingest_book`].
    pub(crate) fn embedder(&self) -> Option<&E> {
        self.library.as_ref().map(Library::embedder)
    }
}

/// Conventional `actor_detail` value for the CLI surface.
pub const ACTOR_DETAIL_CLI: &str = "cli";

/// Conventional `actor_detail` value for the MCP surface.
pub const ACTOR_DETAIL_MCP: &str = "mcp";

/// Conventional `actor_detail` value for control-plane callers
/// reaching the daemon over the local JSON-RPC socket.
pub const ACTOR_DETAIL_CONTROL_PLANE: &str = "control_plane";

/// Conventional `actor_detail` value for the GUI surface.
pub const ACTOR_DETAIL_GUI: &str = "gui";

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

    /// A GUI caller: [`ActorKind::Human`] with `actor_detail = "gui"`.
    pub fn gui() -> Caller {
        Caller {
            actor_kind: ActorKind::Human,
            actor_detail: Some(ACTOR_DETAIL_GUI.to_string()),
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

    /// A control-plane caller: [`ActorKind::Human`] with
    /// `actor_detail = "control_plane"`. Used by JSON-RPC handlers that
    /// reach the runtime business functions on behalf of a local socket
    /// client.
    pub fn control_plane() -> Caller {
        Caller {
            actor_kind: ActorKind::Human,
            actor_detail: Some(ACTOR_DETAIL_CONTROL_PLANE.to_string()),
            session_id: None,
            reason: None,
        }
    }
}
