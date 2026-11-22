// SPDX-License-Identifier: Apache-2.0

//! Confirm the SQLite library linked through rusqlite's `bundled`
//! feature is no older than the release the workspace requires.
//!
//! SQLite 3.54.0 fixes CVE-2026-119375, a read of uninitialized
//! page-cache memory when a database is opened with a sibling `-wal`
//! file whose page size differs. The bundled source comes from the
//! locked `libsqlite3-sys`, so a lockfile that moves it backwards
//! fails here rather than only in the advisory audit.

/// The oldest bundled SQLite the workspace accepts, in the
/// `sqlite3_libversion_number` encoding (`X * 1_000_000 + Y * 1_000 + Z`).
const MIN_SQLITE_VERSION_NUMBER: i32 = 3_054_000;

#[test]
fn the_bundled_sqlite_is_no_older_than_the_required_release() {
    let linked = rusqlite::version_number();
    assert!(
        linked >= MIN_SQLITE_VERSION_NUMBER,
        "the bundled SQLite is {} ({linked}); 3.54.0 or later is required for the \
         CVE-2026-119375 fix",
        rusqlite::version()
    );

    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    let reported: String = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .expect("query sqlite_version()");
    assert_eq!(
        reported,
        rusqlite::version(),
        "the library a connection runs is the one rusqlite links"
    );
}
