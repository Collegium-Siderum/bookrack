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

mod book_state;
mod catalog_meta;
mod db;
mod intake;
mod node_categories;
mod node_contributors;
mod node_overrides;
mod node_publication_attrs;
mod node_reviews;
mod node_role_takeovers;

pub use db::{Catalog, SCHEMA_VERSION};
pub use intake::{Intake, IntakeStatus, NewIntake, Registration};

/// A fallible `catalog` operation.
pub type Result<T> = std::result::Result<T, CatalogError>;

/// Why a `catalog` operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// The underlying SQLite layer reported an error.
    #[error("catalog database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database was built by a different schema revision than this
    /// binary understands. `found` is the raw stored string, since a
    /// foreign database may hold a value that is not a version number.
    #[error("catalog schema mismatch: database reports {found:?}, this build expects v{expected}")]
    SchemaMismatch {
        /// Schema version string recorded in the opened database.
        found: String,
        /// Schema version this binary was compiled against.
        expected: u32,
    },
}
