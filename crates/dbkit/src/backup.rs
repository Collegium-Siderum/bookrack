// SPDX-License-Identifier: Apache-2.0

//! Consistent on-disk database snapshots via SQLite's `VACUUM INTO`.
//!
//! `std::fs::copy` of a live SQLite database can produce a torn
//! snapshot when WAL is in use: the `-wal` and `-shm` sidecars carry
//! committed pages that have not yet been checkpointed into the main
//! file, and a plain copy of the main file alone misses them. `VACUUM
//! INTO` issues a single SQLite-coordinated read transaction and
//! materializes a fully consistent destination database, regardless of
//! concurrent writers.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Copy the SQLite database at `src` into a fresh file at `dst`,
/// producing a consistent snapshot even while `src` is being written.
///
/// Equivalent to `sqlite3 <src> "VACUUM INTO '<dst>'"`. The source is
/// opened read-only (`SQLITE_OPEN_READ_ONLY`) so the call cannot
/// accidentally take a write lock. The destination must not exist;
/// `VACUUM INTO` refuses to overwrite.
pub fn backup_database(src: &Path, dst: &Path) -> rusqlite::Result<()> {
    let src_conn = Connection::open_with_flags(
        src,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    src_conn.execute("VACUUM INTO ?1", [dst.to_string_lossy().as_ref()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusqlite::Connection;
    use tempfile::tempdir;

    #[test]
    fn backup_database_copies_committed_rows() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.db");
        let dst = dir.path().join("dst.db");

        let conn = Connection::open(&src).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL);\
             INSERT INTO t (v) VALUES ('one'), ('two');",
        )
        .unwrap();
        drop(conn);

        backup_database(&src, &dst).unwrap();

        let copy = Connection::open(&dst).unwrap();
        let n: i64 = copy
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn backup_database_captures_wal_committed_rows() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("wal.db");
        let dst = dir.path().join("wal-copy.db");

        let writer = Connection::open(&src).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL);\
                 INSERT INTO t (v) VALUES ('a'), ('b'), ('c');",
            )
            .unwrap();

        // Keep the writer connection alive so the -wal sidecar is not
        // checkpointed away; a plain file copy would miss those pages.
        backup_database(&src, &dst).unwrap();

        let copy = Connection::open(&dst).unwrap();
        let n: i64 = copy
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn backup_database_refuses_existing_destination() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.db");
        let dst = dir.path().join("exists.db");

        Connection::open(&src)
            .unwrap()
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .unwrap();
        std::fs::write(&dst, b"not empty").unwrap();

        let _err = backup_database(&src, &dst).unwrap_err();
    }
}
