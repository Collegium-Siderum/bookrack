// SPDX-License-Identifier: Apache-2.0

//! The reference-book read store.
//!
//! `reference.db` holds the distilled entries of every reference book in
//! the library: one shared `reference_entries` base table keyed by
//! `(book_slug, entry_key)`, an `reference_entry_overlays` layer of user
//! edits, an FTS5 trigram sidecar for full-text lookup, and the
//! `reference_entries_resolved` view that returns the patched payload to
//! callers. The schema lives in [`migrate`]; this entry point exposes the
//! current target version and a thin `open` that runs the migration
//! sequence to the latest version.

use std::path::Path;

use rusqlite::Connection;

pub mod migrate;

pub use migrate::TARGET_VERSION;

/// Errors from opening or migrating `reference.db`.
#[derive(Debug, thiserror::Error)]
pub enum RefsError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
}

/// The crate's `Result` alias.
pub type RefsResult<T> = Result<T, RefsError>;

/// Open `reference.db` at `path` and bring it to [`TARGET_VERSION`].
///
/// The file is created if it does not exist. Migrations are forward-only
/// and idempotent: re-opening an already-migrated database is a no-op.
pub fn open(path: &Path) -> RefsResult<Connection> {
    let mut conn = Connection::open(path)?;
    migrate::migrations().to_latest(&mut conn)?;
    Ok(conn)
}

/// Open `reference.db` in memory and bring it to [`TARGET_VERSION`].
///
/// Convenience for tests and the in-memory MCP / CLI smoke paths.
pub fn open_in_memory() -> RefsResult<Connection> {
    let mut conn = Connection::open_in_memory()?;
    migrate::migrations().to_latest(&mut conn)?;
    Ok(conn)
}
