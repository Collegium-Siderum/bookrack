// SPDX-License-Identifier: Apache-2.0

//! Test fixtures for a `catalog.db` that is behind this build.
//!
//! The migration sequence is private to this crate, so a test in
//! another crate has no other way to put a behind-schema catalog on
//! disk. Nothing here is part of the supported surface.

use std::path::Path;

use rusqlite::Connection;

use crate::migrate::{TARGET_VERSION, migrations};

/// The schema revision this build writes.
// setting: internal -- a test fixture's copy of the schema revision
pub const CURRENT_VERSION: i64 = TARGET_VERSION;

/// Reverses the newest migration on a database that holds its schema.
const NEWEST_MIGRATION_DOWN: &str = "ALTER TABLE pipeline_run_summary DROP COLUMN profile_buckets;";

/// The revision [`NEWEST_MIGRATION_DOWN`] steps back from.
// setting: internal -- a test fixture's record of the revision it reverses
const NEWEST_MIGRATION_DOWN_FROM: i64 = 17;

/// Create a `catalog.db` at `path` whose schema stops at `version`.
///
/// The file holds exactly the tables the first `version` migrations
/// produce and no rows.
///
/// # Panics
///
/// Panics when `version` is outside `1..=CURRENT_VERSION`, when a
/// file already sits at `path`, or on any SQLite failure.
pub fn create_at_version(path: &Path, version: i64) {
    assert!(
        (1..=TARGET_VERSION).contains(&version),
        "version {version} is outside 1..={TARGET_VERSION}",
    );
    assert!(!path.exists(), "{} already exists", path.display());
    let mut conn = Connection::open(path).expect("create catalog fixture");
    let steps = usize::try_from(version).expect("version fits usize");
    migrations()
        .to_version(&mut conn, steps)
        .expect("migrate catalog fixture to the requested version");
}

/// Step the current-revision `catalog.db` at `path` back one revision,
/// keeping its rows.
///
/// The result has the schema and `user_version` of the previous
/// revision, so the next migrating open applies the newest migration
/// for real.
///
/// # Panics
///
/// Panics when the file is not at `CURRENT_VERSION`, when the newest
/// migration has no reverse statement recorded here, or on any SQLite
/// failure.
pub fn step_back_one_version(path: &Path) {
    assert_eq!(
        NEWEST_MIGRATION_DOWN_FROM, TARGET_VERSION,
        "the reverse statement does not belong to the newest migration",
    );
    let conn = Connection::open(path).expect("open catalog fixture");
    assert_eq!(
        read_user_version(&conn),
        TARGET_VERSION,
        "{} is not at the current revision",
        path.display(),
    );
    conn.execute_batch(NEWEST_MIGRATION_DOWN)
        .expect("reverse the newest migration");
    conn.pragma_update(None, "user_version", TARGET_VERSION - 1)
        .expect("write user_version");
}

/// The `user_version` of the database at `path`.
///
/// # Panics
///
/// Panics when the file cannot be opened or read.
pub fn user_version(path: &Path) -> i64 {
    let conn = Connection::open(path).expect("open catalog fixture");
    read_user_version(&conn)
}

fn read_user_version(conn: &Connection) -> i64 {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, NewIntake};
    use bookrack_core::ItemKind;

    #[test]
    fn a_fixture_one_revision_behind_migrates_to_the_current_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        create_at_version(&path, CURRENT_VERSION - 1);
        assert_eq!(user_version(&path), CURRENT_VERSION - 1);

        Catalog::open_for_write_unbacked(&path).expect("a behind fixture migrates and verifies");
        assert_eq!(user_version(&path), CURRENT_VERSION);
    }

    #[test]
    fn stepping_back_keeps_rows_and_leaves_a_migratable_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        let intake_id = Catalog::open_for_write_unbacked(&path)
            .expect("create")
            .register_intake(ItemKind::Book, &NewIntake::new("sha-step-back"))
            .expect("register intake")
            .into_intake()
            .intake_id;

        step_back_one_version(&path);
        assert_eq!(user_version(&path), CURRENT_VERSION - 1);

        let catalog = Catalog::open_for_write_unbacked(&path)
            .expect("the stepped-back file migrates and verifies");
        assert_eq!(user_version(&path), CURRENT_VERSION);
        assert!(
            catalog.intake_by_id(intake_id).expect("lookup").is_some(),
            "the row written before the step back survives it",
        );
    }

    #[test]
    fn a_stepped_back_database_is_refused_by_the_read_only_door() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        Catalog::open_for_write_unbacked(&path).expect("create");
        step_back_one_version(&path);

        assert!(
            Catalog::open_read_only(&path).is_err(),
            "a file whose schema lacks the newest migration must not verify as current",
        );
    }
}
