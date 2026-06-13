// SPDX-License-Identifier: Apache-2.0

//! The `catalog.db` connection handle and schema.

use std::path::{Path, PathBuf};

use bookrack_dbkit::{
    OpenDecision, READER_VERSION, TableSpec, TimedConnection, reader_version_decision,
};
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

/// The `min_reader_version` value this binary stamps when writing
/// `catalog.db`.
///
/// Bump when a writer-side change to `catalog.db` produces content that
/// older binaries could misinterpret — e.g. reinterpreting an enum
/// value or repurposing a column. Adding a column or a table is
/// transparent and does not require a bump.
pub const MIN_READER_VERSION: u32 = 1;

/// `catalog_meta` key under which [`MIN_READER_VERSION`] is recorded.
const MIN_READER_VERSION_KEY: &str = "min_reader_version";

/// How many database backups to retain in the backup directory; older
/// ones are pruned after a successful backup.
const BACKUP_KEEP: usize = 5;

/// Every `catalog.db` table that has a table module of its own, in a
/// stable order. The live schema is conformance-checked against these
/// specs on every open; they are the source of truth for the *current*
/// schema shape, while the migration baseline in [`crate::migrate`] is the
/// historical one.
const SPECS: &[&TableSpec] = &[
    &crate::catalog_meta::SPEC,
    &crate::intake::SPEC,
    &crate::item_state::SPEC,
    &crate::node_publication_attrs::SPEC,
    &crate::node_overrides::SPEC,
    &crate::node_contributors::SPEC,
    &crate::node_role_takeovers::SPEC,
    &crate::node_categories::SPEC,
    &crate::node_reviews::SPEC,
    &crate::metadata_audit::SPEC,
    &crate::item_pipeline_audit::SPEC,
    &crate::works::SPEC,
    &crate::expressions::SPEC,
    &crate::mcp_tool_calls::SPEC,
    &crate::retrieval_issues::SPEC,
    &crate::toc_edits::SPEC,
];

/// A handle to one `catalog.db` database.
///
/// Owns a single SQLite connection. Construct with [`Catalog::open`]
/// for a file-backed database or [`Catalog::open_in_memory`] for an
/// ephemeral one (useful in tests).
pub struct Catalog {
    pub(crate) conn: TimedConnection,
    read_only: bool,
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
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("catalog")
            .to_string();
        Catalog::from_connection(Connection::open(path)?, Some((backup_dir, stem.as_str())))
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
    fn from_connection(mut conn: Connection, backup: Option<(&Path, &str)>) -> Result<Catalog> {
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match decide(current) {
            OpenDecision::Refuse { .. } => {
                return Err(CatalogError::SchemaTooNew {
                    found: current,
                    expected: TARGET_VERSION,
                });
            }
            OpenDecision::Migrate { .. } => {
                // Snapshot only a file-backed database that already holds
                // data and is about to be migrated. A fresh or in-memory
                // database has nothing worth saving.
                if let Some((dir, stem)) = backup
                    && has_user_tables(&conn)?
                {
                    backup_catalog(&conn, dir, stem, current)?;
                }
                // Foreign keys are toggled around the migration, not
                // inside it: a future 12-step table rebuild needs them
                // off, and `PRAGMA foreign_keys` is a no-op within the
                // migration's transaction. `catalog.db` declares none
                // today; the dance keeps the seam ready for one that
                // does.
                conn.pragma_update(None, "foreign_keys", "OFF")?;
                migrations()
                    .to_latest(&mut conn)
                    .map_err(CatalogError::Migrate)?;
                conn.pragma_update(None, "foreign_keys", "ON")?;
            }
            OpenDecision::Match => {}
            // `catalog.db` is source-of-truth and never produces this
            // verdict: the migration framework advances any older revision
            // forward, and there is no derived-stamp axis to disagree on.
            OpenDecision::Rederive { .. } => unreachable!("catalog.db is never rederived"),
        }

        // Acceptance gate, run on every open: `rusqlite_migration` advances
        // `user_version` but does not check the resulting schema shape.
        bookrack_dbkit::verify_all(&conn, SPECS).map_err(CatalogError::Verify)?;

        let catalog = Catalog {
            conn: TimedConnection::new(conn, "catalog"),
            read_only: false,
        };

        // Reader-version axis: refuse a stamp this build cannot meet,
        // seed the stamp when missing. Runs after `verify_all` so the
        // `catalog_meta` table is guaranteed to exist.
        let stored = catalog.read_min_reader_version()?;
        match reader_version_decision(stored) {
            OpenDecision::Refuse { .. } => {
                return Err(CatalogError::ReaderTooOld {
                    required: stored.expect("Refuse implies a stamp was present"),
                    current: READER_VERSION,
                });
            }
            OpenDecision::Match => {
                if stored.is_none() {
                    catalog.meta_set(MIN_READER_VERSION_KEY, &MIN_READER_VERSION.to_string())?;
                }
            }
            OpenDecision::Migrate { .. } | OpenDecision::Rederive { .. } => {
                unreachable!("reader_version_decision emits only Match or Refuse")
            }
        }

        // Mirror the authoritative version into `catalog_meta` for audit.
        catalog.meta_set(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_string())?;
        Ok(catalog)
    }

    /// Read the recorded `min_reader_version` stamp from `catalog_meta`,
    /// returning `None` if no row has been written yet.
    fn read_min_reader_version(&self) -> Result<Option<u32>> {
        Ok(self
            .meta_get(MIN_READER_VERSION_KEY)?
            .and_then(|s| s.parse::<u32>().ok()))
    }

    /// Open the `catalog.db` at `path` for read-only access.
    ///
    /// Skips the migration step entirely — the file must already be at
    /// the current schema revision — and locks the connection with
    /// `PRAGMA query_only = ON`, so any subsequent write through the
    /// resulting handle is rejected by SQLite with `SQLITE_READONLY`.
    ///
    /// Designed for daemon-side query consumers that share one schema
    /// migration owned by a separate read-write entry point at process
    /// start. The `verify_all(SPECS)` acceptance gate still runs, so an
    /// unmigrated or schema-drifted database is refused at open rather
    /// than discovered halfway through a query.
    pub fn open_read_only(path: &Path) -> Result<Catalog> {
        let conn = Connection::open(path)?;
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current > TARGET_VERSION {
            return Err(CatalogError::SchemaTooNew {
                found: current,
                expected: TARGET_VERSION,
            });
        }
        conn.pragma_update(None, "query_only", "ON")?;
        bookrack_dbkit::verify_all(&conn, SPECS).map_err(CatalogError::Verify)?;
        let catalog = Catalog {
            conn: TimedConnection::new(conn, "catalog"),
            read_only: true,
        };
        // Reader-version axis: refuse a stamp this build cannot meet.
        // The seed step from the read-write path is intentionally
        // skipped here — the read-only contract forbids mutating the
        // database. A missing stamp resolves to Match and the open
        // proceeds, leaving seeding to the next read-write open.
        let stored = catalog.read_min_reader_version()?;
        if let OpenDecision::Refuse { .. } = reader_version_decision(stored) {
            return Err(CatalogError::ReaderTooOld {
                required: stored.expect("Refuse implies a stamp was present"),
                current: READER_VERSION,
            });
        }
        Ok(catalog)
    }

    /// Whether this handle was opened read-only.
    ///
    /// `true` only for handles produced by [`Catalog::open_read_only`].
    /// Diagnostic — the actual write barrier is the SQLite
    /// `query_only` PRAGMA, not this flag.
    pub fn is_read_only(&self) -> bool {
        self.read_only
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

/// Classify a catalog database whose `user_version` is `current` into one
/// of the four self-check verdicts the protocol distinguishes.
///
/// `catalog.db` is source-of-truth and rebuildable only by hand, so its
/// decision tree is the minimal one: anything newer is refused, anything
/// older is migrated forward, an exact match is opened unchanged. The
/// rederive verdict is never produced — there is no derived-stamp axis
/// for this store to disagree on.
fn decide(current: i64) -> OpenDecision {
    use std::cmp::Ordering;
    match current.cmp(&TARGET_VERSION) {
        Ordering::Greater => OpenDecision::Refuse {
            reason: "catalog schema version newer than this binary",
        },
        Ordering::Less => OpenDecision::Migrate {
            from: current,
            to: TARGET_VERSION,
        },
        Ordering::Equal => OpenDecision::Match,
    }
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

/// Snapshot the catalog database into `dir` with `VACUUM INTO`, naming
/// the file with the source database's stem, a Zulu timestamp and the
/// version it is migrating from, then prune all but the newest
/// [`BACKUP_KEEP`] backups in the same prefix cluster. Sharing a
/// directory between two catalog databases (e.g. `catalog.db` and
/// `papers_catalog.db`) is safe: each prunes only its own snapshots.
fn backup_catalog(conn: &Connection, dir: &Path, db_stem: &str, from_version: i64) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let ts = now_iso_from(conn)?.replace(':', "-");
    let path = dir.join(format!("{db_stem}-{ts}-from-v{from_version}.bak"));
    let target = path.display().to_string().replace('\'', "''");
    conn.execute(&format!("VACUUM INTO '{target}'"), [])?;
    prune_old_backups(dir, db_stem, BACKUP_KEEP)?;
    Ok(())
}

/// Keep the `keep` newest backups whose filename leads with
/// `<db_stem>-` in `dir`, deleting the rest. Backup filenames embed a
/// sortable timestamp after the stem prefix, so lexical order within
/// one cluster is chronological.
fn prune_old_backups(dir: &Path, db_stem: &str, keep: usize) -> Result<()> {
    let prefix = format!("{db_stem}-");
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
    use bookrack_core::ItemKind;

    #[test]
    fn fresh_database_stamps_the_schema_version() {
        let catalog = Catalog::open_in_memory().expect("open");
        assert_eq!(
            catalog.meta_get(SCHEMA_VERSION_KEY).expect("read"),
            Some(SCHEMA_VERSION.to_string())
        );
    }

    #[test]
    fn fresh_database_stamps_the_min_reader_version() {
        let catalog = Catalog::open_in_memory().expect("open");
        assert_eq!(
            catalog.meta_get(MIN_READER_VERSION_KEY).expect("read"),
            Some(MIN_READER_VERSION.to_string())
        );
    }

    #[test]
    fn open_refuses_a_stamp_above_this_binarys_reader_version() {
        let dir =
            std::env::temp_dir().join(format!("bookrack-catalog-reader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("catalog.db");

        let too_new = READER_VERSION + 1;
        {
            let catalog = Catalog::open(&path).expect("first open");
            catalog
                .meta_set(MIN_READER_VERSION_KEY, &too_new.to_string())
                .expect("overwrite stamp with a too-new value");
        }

        let Err(err) = Catalog::open(&path) else {
            panic!("reopen must refuse")
        };
        assert!(
            matches!(err, CatalogError::ReaderTooOld { required, current }
                if required == too_new && current == READER_VERSION),
            "unexpected error: {err:?}"
        );
        let Err(err) = Catalog::open_read_only(&path) else {
            panic!("read-only reopen must refuse")
        };
        assert!(
            matches!(err, CatalogError::ReaderTooOld { required, current }
                if required == too_new && current == READER_VERSION),
            "unexpected error: {err:?}"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
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
        prune_old_backups(&dir, "catalog", 5).expect("prune");
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
    fn pruning_only_touches_its_own_prefix_cluster() {
        // Two catalog databases sharing a backup directory must each
        // keep `BACKUP_KEEP` of their own snapshots without evicting
        // the other's. The fixture seeds seven backups per prefix.
        let dir = unique_dir("prune-cluster");
        std::fs::create_dir_all(&dir).expect("temp dir");
        for prefix in ["catalog", "papers_catalog"] {
            for i in 0..7 {
                let name = format!("{prefix}-2026-05-31T00-00-0{i}Z-from-v0.bak");
                std::fs::write(dir.join(name), b"x").expect("write backup");
            }
        }
        prune_old_backups(&dir, "catalog", 5).expect("prune catalog");
        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let catalog: Vec<&String> = remaining
            .iter()
            .filter(|n| n.starts_with("catalog-"))
            .collect();
        let papers: Vec<&String> = remaining
            .iter()
            .filter(|n| n.starts_with("papers_catalog-"))
            .collect();
        assert_eq!(catalog.len(), 5, "pruning catalog must keep five");
        assert_eq!(
            papers.len(),
            7,
            "the papers cluster must not be touched by pruning catalog"
        );

        prune_old_backups(&dir, "papers_catalog", 5).expect("prune papers_catalog");
        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let catalog: Vec<&String> = remaining
            .iter()
            .filter(|n| n.starts_with("catalog-"))
            .collect();
        let papers: Vec<&String> = remaining
            .iter()
            .filter(|n| n.starts_with("papers_catalog-"))
            .collect();
        assert_eq!(catalog.len(), 5, "the catalog cluster must remain intact");
        assert_eq!(papers.len(), 5, "pruning papers_catalog must keep five");

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
    fn open_read_only_rejects_writes() {
        use crate::{IntakeStatus, NewIntake};

        let dir = unique_dir("read-only-blocks-writes");
        let path = dir.join("catalog.db");
        {
            // Initialize the schema through the read-write entry point.
            let mut catalog = Catalog::open(&path).expect("first open");
            catalog
                .register_intake(ItemKind::Book, &NewIntake::new("sha-rw"))
                .expect("seed");
        }

        let read_only = Catalog::open_read_only(&path).expect("open read-only");
        assert!(read_only.is_read_only());

        // The existing row is still readable.
        let by_sha = read_only
            .intake_by_sha("sha-rw")
            .expect("read existing")
            .expect("present");
        assert_eq!(by_sha.status, IntakeStatus::Pending);

        // But any write through this handle fails with a SQLite error.
        let mut writer = read_only;
        let err = writer
            .register_intake(ItemKind::Book, &NewIntake::new("sha-blocked"))
            .expect_err("write must fail");
        assert!(matches!(err, CatalogError::Sqlite(_)), "{err:?}");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn open_read_only_is_idempotent_against_an_initialized_database() {
        let dir = unique_dir("read-only-reads");
        let path = dir.join("catalog.db");
        Catalog::open(&path).expect("first open");
        let read_only = Catalog::open_read_only(&path).expect("read-only open");
        // The acceptance gate ran: the version mirror is still readable.
        let version = read_only.meta_get(SCHEMA_VERSION_KEY).expect("read");
        assert_eq!(version, Some(SCHEMA_VERSION.to_string()));
        assert!(read_only.is_read_only());

        std::fs::remove_dir_all(&dir).expect("cleanup");
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
