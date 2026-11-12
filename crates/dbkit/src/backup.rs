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

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// How many pre-migration backups of one database to keep in a backup
/// directory; older ones are pruned after a successful backup.
// setting: db.backups_kept
pub(crate) const BACKUP_KEEP: usize = 5;

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

/// The current UTC time as a Zulu ISO-8601 string, read from SQLite's
/// clock so that it shares one source with the timestamps a table's
/// own inserts generate.
// Parallel implementation: rfc3339-now.
pub fn now_iso(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |row| {
        row.get(0)
    })
}

/// Whether the database holds any table of its own, as opposed to
/// being a freshly created, empty file.
pub(crate) fn has_user_tables(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Snapshot the database behind `conn` into `dir` before it migrates.
///
/// The file is named `<stem>-<timestamp>-from-v<version>.bak`. Once it
/// is written, all but the newest [`BACKUP_KEEP`] snapshots that lead
/// with the same stem are pruned, so two databases sharing a directory
/// each keep their own.
pub(crate) fn backup_before_migration(
    conn: &Connection,
    dir: &Path,
    stem: &str,
    from_version: i64,
) -> Result<(), crate::store::OpenError> {
    std::fs::create_dir_all(dir)?;
    let ts = now_iso(conn)?.replace(':', "-");
    let path = dir.join(format!("{stem}-{ts}-from-v{from_version}.bak"));
    conn.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
    prune_old_backups(dir, stem, BACKUP_KEEP)?;
    Ok(())
}

/// Keep the `keep` newest backups whose filename leads with `<stem>-`
/// in `dir`, deleting the rest. A backup filename carries a sortable
/// timestamp after the stem, so lexical order within one stem is
/// chronological.
fn prune_old_backups(dir: &Path, stem: &str, keep: usize) -> std::io::Result<()> {
    let prefix = format!("{stem}-");
    let mut backups: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".bak"))
        })
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(keep);
    for path in &backups[..excess] {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusqlite::Connection;
    use tempfile::tempdir;

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn pruning_keeps_only_the_newest_backups() {
        let dir = tempdir().unwrap();
        // Timestamp-led names sort chronologically; make seven.
        for i in 0..7 {
            let name = format!("catalog-2026-05-31T00-00-0{i}Z-from-v0.bak");
            std::fs::write(dir.path().join(name), b"x").expect("write backup");
        }
        prune_old_backups(dir.path(), "catalog", 5).expect("prune");
        let remaining = file_names(dir.path());
        assert_eq!(remaining.len(), 5);
        // The two oldest (seconds 00, 01) are gone; 02..06 remain.
        assert!(
            remaining
                .iter()
                .all(|n| !n.contains("-00Z-") && !n.contains("-01Z-"))
        );
        assert!(remaining.iter().any(|n| n.contains("-06Z-")));
    }

    #[test]
    fn pruning_only_touches_its_own_prefix_cluster() {
        // Two databases sharing a backup directory each keep their own
        // snapshots without evicting the other's.
        let dir = tempdir().unwrap();
        for prefix in ["catalog", "papers_catalog"] {
            for i in 0..7 {
                let name = format!("{prefix}-2026-05-31T00-00-0{i}Z-from-v0.bak");
                std::fs::write(dir.path().join(name), b"x").expect("write backup");
            }
        }
        let count = |lead: &str| {
            file_names(dir.path())
                .iter()
                .filter(|n| n.starts_with(lead))
                .count()
        };

        prune_old_backups(dir.path(), "catalog", 5).expect("prune catalog");
        assert_eq!(count("catalog-"), 5, "pruning catalog must keep five");
        assert_eq!(
            count("papers_catalog-"),
            7,
            "the papers cluster must not be touched by pruning catalog"
        );

        prune_old_backups(dir.path(), "papers_catalog", 5).expect("prune papers_catalog");
        assert_eq!(
            count("catalog-"),
            5,
            "the catalog cluster must remain intact"
        );
        assert_eq!(
            count("papers_catalog-"),
            5,
            "pruning papers_catalog must keep five"
        );
    }

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
    fn backup_database_snapshots_the_committed_state_mid_transaction() {
        // The "even while being written" half of the contract: a writer
        // holding an open transaction must neither block the snapshot
        // nor leak its uncommitted rows into it.
        let dir = tempdir().unwrap();
        let src = dir.path().join("live.db");
        let during = dir.path().join("during.db");
        let after = dir.path().join("after.db");

        let writer = Connection::open(&src).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL);\
                 INSERT INTO t (v) VALUES ('a'), ('b');",
            )
            .unwrap();

        // Open a write transaction and leave it open across the backup.
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
            .execute("INSERT INTO t (v) VALUES ('c')", [])
            .unwrap();

        backup_database(&src, &during).expect("a live writer must not block the snapshot");

        let copy = Connection::open(&during).unwrap();
        let rows: Vec<String> = copy
            .prepare("SELECT v FROM t ORDER BY v")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec!["a".to_string(), "b".to_string()],
            "the snapshot carries the committed state, without the open transaction's row",
        );

        // Once the transaction commits, the next snapshot carries it —
        // so the assertion above is about isolation, not about the row
        // being unwritable.
        writer.execute_batch("COMMIT").unwrap();
        backup_database(&src, &after).unwrap();
        let n: i64 = Connection::open(&after)
            .unwrap()
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
