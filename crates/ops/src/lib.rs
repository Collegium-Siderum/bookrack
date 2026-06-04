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

    /// A search op was issued on an [`Ops`] built without a vector store.
    #[error("search is not available on a catalog-only Ops handle")]
    SearchUnavailable,
}

/// A fallible op.
pub type Result<T> = std::result::Result<T, OpsError>;

/// Warm, shareable op state.
///
/// Holds the file-system paths every op needs and, optionally, a warm
/// [`Library`] for search.
pub struct Ops<E: Embedder> {
    library: Option<Library<E>>,
    corpus_db: PathBuf,
    catalog_db: PathBuf,
    #[allow(dead_code)] // Wired through for later phases that touch vectors.
    lancedb_dir: PathBuf,
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
        caller: Caller,
    ) -> Ops<E> {
        Ops {
            library: Some(library),
            corpus_db,
            catalog_db,
            lancedb_dir: lancedb_dir.to_path_buf(),
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
        caller: Caller,
    ) -> Ops<E> {
        Ops {
            library: None,
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
