// SPDX-License-Identifier: Apache-2.0

//! `catalog.db` — identity, curated metadata, audit, and FRBR.
//!
//! This crate owns the source-of-truth half of the data model. Unlike
//! `corpus.db`, `catalog.db` cannot be rebuilt from the source files:
//! it holds the file-intake registry, human-curated metadata and
//! corrections, the audit trail, the manual TOC-edit overlay, and the
//! FRBR identity tables. It is backed up independently.
//!
//! `catalog.db` references the `corpus.db` node tree only by bare
//! integer `node_id` — a soft reference, with no cross-database foreign
//! key. Even its own intra-database links (an expression to its work,
//! a book to its intake) are soft integer references, so the two
//! databases stay independently movable and restorable.
//!
//! All SQL is contained in this crate; callers work through the typed
//! [`Catalog`] handle.

mod actor;
mod book_pipeline_audit;
mod book_state;
mod catalog_meta;
mod db;
mod effective;
mod expressions;
mod intake;
mod mcp_tool_calls;
mod metadata_audit;
mod migrate;
mod node_categories;
mod node_contributors;
mod node_overrides;
mod node_publication_attrs;
mod node_reviews;
mod node_role_takeovers;
mod retrieval_issues;
mod works;

pub use actor::ActorKind;
pub use book_pipeline_audit::{BookPipelineAudit, NewBookPipelineAudit};
pub use book_state::{BookState, NewBookState};
pub use db::{Catalog, SCHEMA_VERSION};
pub use effective::EffectiveAttrs;
pub use expressions::{Expression, NewExpression};
pub use intake::{Intake, IntakeStatus, NewIntake, Registration};
pub use mcp_tool_calls::{McpToolCall, NewMcpToolCall};
pub use metadata_audit::{MetadataAudit, NewMetadataAudit};
pub use node_categories::{NewCategory, NodeCategory};
pub use node_contributors::{NewContributor, NodeContributor};
pub use node_overrides::{NewOverride, NodeOverride};
pub use node_publication_attrs::{NewPublicationAttrs, PublicationAttrs};
pub use node_reviews::{NewReview, NodeReview};
pub use node_role_takeovers::{NewRoleTakeover, NodeRoleTakeover};
pub use retrieval_issues::{NewRetrievalIssue, RetrievalIssue};
pub use works::{NewWork, Work};

/// A fallible `catalog` operation.
pub type Result<T> = std::result::Result<T, CatalogError>;

/// Why a `catalog` operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// The underlying SQLite layer reported an error.
    #[error("catalog database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database was written by a newer schema revision than this
    /// binary understands: its `user_version` exceeds the highest
    /// migration defined. Rather than downgrade it, opening fails so the
    /// operator can run a newer build or restore a backup.
    #[error(
        "catalog schema is newer than this build: database is at v{found}, \
         this build understands up to v{expected}"
    )]
    SchemaTooNew {
        /// `user_version` recorded in the opened database.
        found: i64,
        /// Highest schema version this binary defines.
        expected: i64,
    },

    /// A schema migration failed to apply.
    #[error("catalog migration failed: {0}")]
    Migrate(#[from] rusqlite_migration::Error),

    /// The migrated schema does not match the compiled-in specs.
    #[error("catalog schema verification failed: {0}")]
    Verify(#[from] bookrack_dbkit::VerifyError),

    /// A filesystem error while writing or pruning a database backup.
    #[error("catalog backup error: {0}")]
    Io(#[from] std::io::Error),
}
