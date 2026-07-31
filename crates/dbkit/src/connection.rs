// SPDX-License-Identifier: Apache-2.0

//! Production SQLite open helpers.
//!
//! Every database-owning crate routes its `open()` and `open_read_only()`
//! entry points through this module, so the per-connection PRAGMAs the
//! production runtime needs are applied in one place rather than at each
//! store. Two roles are distinguished:
//!
//! - [`open_production`] — read-write handles. Sets `journal_mode = WAL`
//!   so readers see a committed snapshot while a writer is mid-transaction.
//!   `WAL` is a persistent file-level mode: setting it once is enough,
//!   but re-applying on every writer open is a SQLite no-op and keeps the
//!   contract local to the helper.
//! - [`open_production_strict_read_only`] — `SQLITE_OPEN_READ_ONLY |
//!   SQLITE_OPEN_NO_MUTEX`, plus `query_only = ON`. The strict form
//!   refuses to materialize the database on open; a read-only
//!   connection to an existing WAL database can still create the
//!   `-shm` / `-wal` sidecars itself, so no wider flag set is needed
//!   for the read role.
//!
//! Every helper also installs a `busy_timeout`: WAL widens the
//! read-while-write window but cannot remove the brief PENDING /
//! EXCLUSIVE upgrades SQLite still needs around checkpoint, DDL,
//! `VACUUM`, and large-transaction commits. The timeout lets reads
//! drain through those windows instead of bubbling up as
//! `SQLITE_BUSY`.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

/// How long a connection waits on a busy lock before giving up.
///
/// Five seconds covers the upper bound of the writer-side commit
/// windows the ingest worker produces in practice. A reader that
/// blocks for longer is almost certainly waiting on a stuck writer,
/// not on a normal commit.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Open a read-write SQLite connection at `path` and apply the
/// production PRAGMA set.
///
/// Sets `journal_mode = WAL` and a `busy_timeout`. The default
/// `OpenFlags` are used, so the file is created if missing and the
/// connection is `SQLITE_OPEN_FULL_MUTEX` — the same threading
/// posture each store had before this helper was introduced.
pub fn open_production(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Open a strictly read-only SQLite connection at `path`.
///
/// Uses `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`, so the file
/// must already exist and the connection cannot create the WAL
/// sidecars itself. Suitable for stores that already promise this
/// contract to their callers. Also installs `query_only = ON` as a
/// defence-in-depth against PRAGMA mutations and a `busy_timeout`.
pub fn open_production_strict_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "query_only", "ON")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use tempfile::tempdir;

    fn pragma_str(conn: &Connection, name: &str) -> String {
        conn.pragma_query_value(None, name, |row| row.get::<_, String>(0))
            .expect("read pragma")
    }

    fn pragma_i64(conn: &Connection, name: &str) -> i64 {
        conn.pragma_query_value(None, name, |row| row.get::<_, i64>(0))
            .expect("read pragma")
    }

    #[test]
    fn writer_open_sets_wal_and_busy_timeout() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("writer.db");
        let conn = open_production(&path).expect("open writer");
        assert_eq!(pragma_str(&conn, "journal_mode"), "wal");
        assert!(pragma_i64(&conn, "busy_timeout") > 0);
    }

    #[test]
    fn strict_read_only_open_refuses_a_missing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("absent.db");

        open_production_strict_read_only(&path).expect_err("missing file must not open");
        assert!(
            std::fs::read_dir(dir.path())
                .expect("read dir")
                .next()
                .is_none(),
            "a refused open must leave the directory untouched",
        );
    }

    #[test]
    fn strict_read_only_open_refuses_writes() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("strict.db");
        drop(open_production(&path).expect("seed writer"));

        let conn = open_production_strict_read_only(&path).expect("open strict reader");
        assert_eq!(pragma_i64(&conn, "query_only"), 1);
        assert!(pragma_i64(&conn, "busy_timeout") > 0);
        let err = conn
            .execute("CREATE TABLE t (x INTEGER)", [])
            .expect_err("strict reader must reject writes");
        assert!(
            format!("{err}").to_lowercase().contains("readonly"),
            "expected readonly rejection, got {err}",
        );
    }

    /// Lock-model regression: with a writer holding an EXCLUSIVE
    /// transaction, a strict read-only reader must still complete
    /// inside the writer's hold window thanks to WAL plus the busy
    /// timeout.
    ///
    /// The same writer in `journal_mode = delete` blocks the reader
    /// for the entire hold (no committed snapshot to read from), so
    /// this test fails closed if the production PRAGMAs ever stop
    /// being applied.
    #[test]
    fn reader_completes_during_writer_exclusive_hold() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("locks.db");
        {
            let seed = open_production(&path).expect("seed writer");
            seed.execute_batch(
                "CREATE TABLE t (x INTEGER);
                 INSERT INTO t (x) VALUES (1), (2), (3);",
            )
            .expect("seed schema");
        }

        let writer_path = path.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let conn = open_production(&writer_path).expect("writer open");
            conn.execute_batch("BEGIN EXCLUSIVE;").expect("begin");
            // Hold for slightly less than the busy timeout so the
            // reader's wait, if any, has room to drain.
            ready_tx.send(()).expect("notify ready");
            thread::sleep(Duration::from_millis(1_500));
            conn.execute_batch("COMMIT;").expect("commit");
        });

        ready_rx.recv().expect("writer ready");

        let start = Instant::now();
        let reader = open_production_strict_read_only(&path).expect("reader open");
        let sum: i64 = reader
            .query_row("SELECT sum(x) FROM t", [], |row| row.get(0))
            .expect("reader read");
        let elapsed = start.elapsed();

        writer.join().expect("writer join");
        assert_eq!(sum, 6);
        assert!(
            elapsed < BUSY_TIMEOUT,
            "reader took {elapsed:?}, must drain inside busy timeout",
        );
    }
}
