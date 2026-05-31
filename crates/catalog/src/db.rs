// SPDX-License-Identifier: Apache-2.0

//! The `catalog.db` connection handle and schema.

use std::path::{Path, PathBuf};

use bookrack_dbkit::{TableSpec, TimedConnection};
use rusqlite::Connection;

use crate::migrate::{TARGET_VERSION, migrations};
use crate::{CatalogError, Result};

/// The `catalog_meta.schema_version` value this binary writes after a
/// successful open.
///
/// It mirrors the authoritative `user_version`
/// ([`TARGET_VERSION`](crate::migrate::TARGET_VERSION)), kept for audit and
/// for any tool or human still reading the old stamp. The `user_version`,
/// not this, decides whether a database can be opened.
pub const SCHEMA_VERSION: u32 = TARGET_VERSION as u32;

/// `catalog_meta` key under which [`SCHEMA_VERSION`] is mirrored.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// How many database backups to retain in the backup directory; older
/// ones are pruned after a successful backup.
const BACKUP_KEEP: usize = 5;

/// Every `catalog.db` table that has a table module of its own, in a
/// stable order. The live schema is conformance-checked against these
/// specs on every open; they are the source of truth for the *current*
/// schema shape, while the migration baseline in [`crate::migrate`] is the
/// historical one. `toc_edits` has no spec yet (it is created by the
/// migration baseline) and so is not covered here.
const SPECS: &[&TableSpec] = &[
    &crate::catalog_meta::SPEC,
    &crate::intake::SPEC,
    &crate::book_state::SPEC,
    &crate::node_publication_attrs::SPEC,
    &crate::node_overrides::SPEC,
    &crate::node_contributors::SPEC,
    &crate::node_role_takeovers::SPEC,
    &crate::node_categories::SPEC,
    &crate::node_reviews::SPEC,
    &crate::metadata_audit::SPEC,
    &crate::book_pipeline_audit::SPEC,
    &crate::works::SPEC,
    &crate::expressions::SPEC,
    &crate::mcp_tool_calls::SPEC,
    &crate::retrieval_issues::SPEC,
];

/// A handle to one `catalog.db` database.
///
/// Owns a single SQLite connection. Construct with [`Catalog::open`]
/// for a file-backed database or [`Catalog::open_in_memory`] for an
/// ephemeral one (useful in tests).
pub struct Catalog {
    pub(crate) conn: TimedConnection,
}

impl Catalog {
    /// Open the `catalog.db` at `path`, creating and initializing it if it
    /// does not exist, without taking a pre-migration backup.
    ///
    /// Use [`Catalog::open_with_backup`] for the production path, which
    /// snapshots an existing database before migrating it. This variant is
    /// for callers that manage backups themselves or do not need them.
    pub fn open(path: &Path) -> Result<Catalog> {
        Catalog::from_connection(Connection::open(path)?, None)
    }

    /// Open the `catalog.db` at `path`, backing it up into `backup_dir`
    /// before applying any pending migration.
    ///
    /// Only an existing, populated database that is actually about to be
    /// migrated is backed up; a freshly created one is not. Fails with
    /// [`CatalogError::SchemaTooNew`] if the file was written by a newer
    /// schema revision than this binary understands.
    pub fn open_with_backup(path: &Path, backup_dir: &Path) -> Result<Catalog> {
        Catalog::from_connection(Connection::open(path)?, Some(backup_dir))
    }

    /// Open an ephemeral, private `catalog.db` held entirely in memory.
    /// The database vanishes when the handle is dropped.
    pub fn open_in_memory() -> Result<Catalog> {
        Catalog::from_connection(Connection::open_in_memory()?, None)
    }

    /// Migrate the schema to the current revision and return a handle.
    ///
    /// `user_version` is authoritative: a database newer than this binary
    /// is refused, a database behind is backed up (when `backup_dir` is
    /// given and it already holds data) and migrated forward, and one
    /// already current opens unchanged. After migrating, the live schema
    /// is verified against the specs and the version mirror is rewritten.
    fn from_connection(mut conn: Connection, backup_dir: Option<&Path>) -> Result<Catalog> {
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        // Refuse a database written by a newer binary rather than
        // downgrading it: the operator runs a newer build or restores a
        // backup.
        if current > TARGET_VERSION {
            return Err(CatalogError::SchemaTooNew {
                found: current,
                expected: TARGET_VERSION,
            });
        }

        // Snapshot only a file-backed database that already holds data and
        // is about to be migrated. A fresh or in-memory database has
        // nothing worth saving.
        if let Some(dir) = backup_dir
            && current < TARGET_VERSION
            && has_user_tables(&conn)?
        {
            backup_catalog(&conn, dir, current)?;
        }

        // Foreign keys are toggled around the migration, not inside it: a
        // future 12-step table rebuild needs them off, and
        // `PRAGMA foreign_keys` is a no-op within the migration's
        // transaction. `catalog.db` declares none today; the dance keeps
        // the seam ready for one that does.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        migrations()
            .to_latest(&mut conn)
            .map_err(CatalogError::Migrate)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Acceptance gate, run on every open: `rusqlite_migration` advances
        // `user_version` but does not check the resulting schema shape. The
        // pending tables carry no spec yet and so are not covered.
        bookrack_dbkit::verify_all(&conn, SPECS).map_err(CatalogError::Verify)?;

        let catalog = Catalog {
            conn: TimedConnection::new(conn, "catalog"),
        };
        // Mirror the authoritative version into `catalog_meta` for audit.
        catalog.meta_set(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_string())?;
        Ok(catalog)
    }

    /// The current UTC time as an ISO-8601 string, read from SQLite's
    /// clock so timestamps the crate writes share one source with the
    /// `ts` columns the table inserts generate themselves.
    pub fn now_iso(&self) -> Result<String> {
        Ok(now_iso_from(&self.conn)?)
    }
}

/// The current UTC time as a Zulu ISO-8601 string, read from a raw
/// connection (before a [`Catalog`] handle exists, e.g. while naming a
/// backup file).
fn now_iso_from(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |row| {
        row.get(0)
    })
}

/// Whether the database holds any non-internal table — i.e. it is not a
/// freshly created, empty file.
fn has_user_tables(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Snapshot the catalog database into `dir` with `VACUUM INTO`, naming the
/// file with a Zulu timestamp and the version it is migrating from, then
/// prune all but the newest [`BACKUP_KEEP`] backups.
fn backup_catalog(conn: &Connection, dir: &Path, from_version: i64) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    // Timestamp first so a lexical sort of the filenames is chronological.
    // ':' is replaced because it is not portable in filenames.
    let ts = now_iso_from(conn)?.replace(':', "-");
    let path = dir.join(format!("catalog-{ts}-from-v{from_version}.bak"));
    // VACUUM INTO takes no bind parameters; escape any single quote in the
    // path so it cannot break out of the SQL string literal.
    let target = path.display().to_string().replace('\'', "''");
    conn.execute(&format!("VACUUM INTO '{target}'"), [])?;
    prune_old_backups(dir, BACKUP_KEEP)?;
    Ok(())
}

/// Keep the `keep` newest catalog backups in `dir`, deleting the rest.
/// Backup filenames lead with a sortable timestamp, so lexical order is
/// chronological.
fn prune_old_backups(dir: &Path, keep: usize) -> Result<()> {
    let mut backups: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("catalog-") && name.ends_with(".bak"))
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

    #[test]
    fn fresh_database_stamps_the_schema_version() {
        let catalog = Catalog::open_in_memory().expect("open");
        assert_eq!(
            catalog.meta_get(SCHEMA_VERSION_KEY).expect("read"),
            Some(SCHEMA_VERSION.to_string())
        );
    }

    #[test]
    fn opening_is_idempotent() {
        // Re-running the schema batch against an initialized database
        // must neither fail nor disturb the recorded version. This needs
        // a real file, since each in-memory database is distinct.
        let dir = std::env::temp_dir().join(format!("bookrack-catalog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("catalog.db");

        Catalog::open(&path).expect("first open");
        // Scope the reopened handle so its connection is closed before
        // the cleanup: Windows refuses to delete a file still held open.
        let version = {
            let reopened = Catalog::open(&path).expect("second open");
            reopened.meta_get(SCHEMA_VERSION_KEY).expect("read")
        };
        assert_eq!(version, Some(SCHEMA_VERSION.to_string()));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_newer_schema_version_is_refused() {
        // A database whose user_version exceeds the highest migration this
        // binary defines must be refused, not downgraded.
        let dir = unique_dir("refuse-newer");
        let path = dir.join("catalog.db");
        {
            let catalog = Catalog::open(&path).expect("first open");
            catalog
                .conn
                .pragma_update(None, "user_version", TARGET_VERSION + 1)
                .expect("bump user_version");
        }
        let Err(err) = Catalog::open(&path) else {
            panic!("must refuse a newer database");
        };
        assert!(matches!(err, CatalogError::SchemaTooNew { .. }), "{err:?}");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn an_existing_populated_database_is_adopted_and_backed_up() {
        // A database with the pre-framework tables but user_version 0 is
        // adopted in place: backed up, then migrated forward — through the
        // address migration — to the current revision.
        let dir = unique_dir("adopt");
        let path = dir.join("catalog.db");
        let backup_dir = dir.join("backup");
        {
            // Build a genuine pre-address schema: migrate only to v2, the
            // baseline plus the contributor index, so the node tables still
            // carry the bare node_id the address migration later replaces.
            let mut conn = Connection::open(&path).expect("seed connection");
            migrations()
                .to_version(&mut conn, 2)
                .expect("seed to the pre-address revision");
            // Simulate a database created before the migration framework:
            // drop the index the first migration added and clear the
            // recorded version so adoption replays from the start.
            conn.execute_batch("DROP INDEX idx_contrib_node")
                .expect("drop the post-baseline index");
            conn.pragma_update(None, "user_version", 0)
                .expect("reset user_version");
        }
        {
            let adopted =
                Catalog::open_with_backup(&path, &backup_dir).expect("adopt existing database");
            let version: i64 = adopted
                .conn
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("read user_version");
            assert_eq!(version, TARGET_VERSION);
        }
        // Exactly one backup was taken, and it is a readable database.
        let backups: Vec<_> = std::fs::read_dir(&backup_dir)
            .expect("read backup dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        Connection::open(&backups[0]).expect("backup must open");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_fresh_database_is_not_backed_up() {
        // Creating a new database has nothing to snapshot, so no backup
        // file is written even when a backup directory is given.
        let dir = unique_dir("fresh-no-backup");
        let path = dir.join("catalog.db");
        let backup_dir = dir.join("backup");
        Catalog::open_with_backup(&path, &backup_dir).expect("fresh open");
        let empty = !backup_dir.exists()
            || std::fs::read_dir(&backup_dir)
                .expect("read backup dir")
                .next()
                .is_none();
        assert!(empty, "a fresh database must not be backed up");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn pruning_keeps_only_the_newest_backups() {
        let dir = unique_dir("prune");
        std::fs::create_dir_all(&dir).expect("temp dir");
        // Timestamp-led names sort chronologically; make seven.
        for i in 0..7 {
            let name = format!("catalog-2026-05-31T00-00-0{i}Z-from-v0.bak");
            std::fs::write(dir.join(name), b"x").expect("write backup");
        }
        prune_old_backups(&dir, 5).expect("prune");
        let mut remaining: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();
        assert_eq!(remaining.len(), 5);
        // The two oldest (seconds 00, 01) are gone; 02..06 remain.
        assert!(
            remaining
                .iter()
                .all(|n| !n.contains("-00Z-") && !n.contains("-01Z-"))
        );
        assert!(remaining.iter().any(|n| n.contains("-06Z-")));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_contributor_index_migration_is_applied() {
        // The first real migration (M[1]) adds idx_contrib_node on top of
        // the frozen baseline; a freshly opened database must carry it.
        let catalog = Catalog::open_in_memory().expect("open");
        let present: i64 = catalog
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_contrib_node'",
                [],
                |row| row.get(0),
            )
            .expect("query index");
        assert_eq!(present, 1);
    }

    #[test]
    fn the_built_schema_conforms_to_every_spec() {
        // Proves the DDL rendered from the specs builds a database whose
        // live schema matches those same specs.
        let catalog = Catalog::open_in_memory().expect("open");
        bookrack_dbkit::verify_all(&catalog.conn, SPECS)
            .expect("the rendered schema must conform to every spec");
    }

    #[test]
    fn now_iso_returns_a_zulu_timestamp() {
        let catalog = Catalog::open_in_memory().expect("open");
        let ts = catalog.now_iso().expect("now");
        // Shape check: `YYYY-MM-DDTHH:MM:SSZ`.
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z'), "{ts}");
        assert_eq!(ts.as_bytes()[10], b'T', "{ts}");
    }

    /// A unique temp directory for a test that needs a real file, tagged so
    /// parallel tests do not collide.
    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bookrack-catalog-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }
}
