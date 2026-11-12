// SPDX-License-Identifier: Apache-2.0

//! Test fixtures for a `reference.db` that is behind this build.
//!
//! A test in another crate has no other way to put a store an earlier
//! release wrote on disk. Nothing here is part of the supported surface.

use std::path::Path;

use rusqlite::Connection;

use crate::migrate::{TARGET_VERSION, migrations};

/// Create a `reference.db` at `path` whose schema stops at `version`.
///
/// The file holds exactly the objects the first `version` migrations
/// produce and no rows.
///
/// # Panics
///
/// Panics when `version` is outside `1..=TARGET_VERSION`, when a file
/// already sits at `path`, or on any SQLite failure.
pub fn create_at_version(path: &Path, version: i64) {
    assert!(
        (1..=TARGET_VERSION).contains(&version),
        "version {version} is outside 1..={TARGET_VERSION}",
    );
    assert!(!path.exists(), "{} already exists", path.display());
    let mut conn = Connection::open(path).expect("create reference fixture");
    let steps = usize::try_from(version).expect("version fits usize");
    migrations()
        .to_version(&mut conn, steps)
        .expect("migrate reference fixture to the requested version");
}

/// The `user_version` of the database at `path`.
///
/// # Panics
///
/// Panics when the file cannot be opened or read.
pub fn user_version(path: &Path) -> i64 {
    let conn = Connection::open(path).expect("open reference fixture");
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version")
}
