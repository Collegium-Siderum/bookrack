// SPDX-License-Identifier: Apache-2.0

//! The `catalog.db` connection handle and schema.

use std::path::Path;

use bookrack_dbkit::{
    Access, OpenError, Opened, ReaderStamp, SchemaAxis, StoreSpec, TableSpec, TimedConnection,
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
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
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
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const MIN_READER_VERSION: u32 = 1;

/// `catalog_meta` key under which [`MIN_READER_VERSION`] is recorded.
const MIN_READER_VERSION_KEY: &str = "min_reader_version";

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
    &crate::book_distill_audit::SPEC,
    &crate::book_distill_audit::STAGE_SPEC,
    &crate::node_paper_audit::SPEC,
    &crate::pipeline_runs::SPEC,
    &crate::pipeline_run_summary::SPEC,
    &crate::retrieval_calls::SPEC,
    &crate::retrieval_call_hits::SPEC,
];

/// `catalog.db` as the open protocol sees it. Both catalogs of a
/// library share it; a backup is named after the file it snapshots.
const STORE: StoreSpec = StoreSpec {
    label: "catalog",
    tables: SPECS,
    schema: SchemaAxis::Migrated {
        target: TARGET_VERSION,
        migrate: migrate_to_latest,
        retired: Some(crate::retired::retired_labels),
    },
    reader_stamp: ReaderStamp {
        table: "catalog_meta",
        key: MIN_READER_VERSION_KEY,
        write_version: MIN_READER_VERSION,
    },
};

fn migrate_to_latest(conn: &mut Connection) -> std::result::Result<(), rusqlite_migration::Error> {
    migrations().to_latest(conn)
}

impl From<OpenError> for CatalogError {
    fn from(e: OpenError) -> CatalogError {
        match e {
            OpenError::TooOld { found, target } => CatalogError::SchemaTooOld {
                found,
                expected: target,
            },
            OpenError::TooNew { found, target } => CatalogError::SchemaTooNew {
                found,
                expected: target,
            },
            OpenError::ReaderTooOld { required, current } => {
                CatalogError::ReaderTooOld { required, current }
            }
            OpenError::Verify(e) => CatalogError::Verify(e),
            OpenError::Migrate(e) => CatalogError::Migrate(e),
            OpenError::Sqlite(e) => CatalogError::Sqlite(e),
            OpenError::Io(e) => CatalogError::Io(e),
            OpenError::Rederive { .. } => unreachable!("catalog.db is migrated, never rederived"),
            OpenError::Retired { .. } => {
                unreachable!("a refusal for retired labels is converted with its path")
            }
        }
    }
}

/// Convert a refusal to open the file at `path`.
fn refused(path: &Path) -> impl FnOnce(OpenError) -> CatalogError + '_ {
    move |e| match e {
        OpenError::Retired { hits, .. } => CatalogError::RetiredLabels {
            path: path.to_path_buf(),
            hits,
        },
        other => other.into(),
    }
}

/// A handle to one `catalog.db` database.
///
/// Owns a single SQLite connection. Three kinds of door produce one:
/// [`Catalog::open`] and [`Catalog::open_read_only`] look,
/// [`Catalog::open_for_bookkeeping`] writes into a catalog that is
/// already current, and [`Catalog::open_for_write`] creates or migrates.
/// [`Catalog::open_in_memory`] gives an ephemeral one.
pub struct Catalog {
    pub(crate) conn: TimedConnection,
    read_only: bool,
}

impl Catalog {
    /// Open the `catalog.db` at `path` read-only, if there is one.
    ///
    /// A pipeline's catalog is created by that pipeline's first write,
    /// so a library may legitimately have no file at `path` yet:
    /// `Ok(None)` when the filesystem reports none. Looking never
    /// creates the file, never migrates it, and never writes a stamp.
    ///
    /// A catalog behind this build is refused with
    /// [`CatalogError::RetiredLabels`] when it holds rows carrying
    /// retired labels and with [`CatalogError::SchemaTooOld`]
    /// otherwise, one ahead of it with
    /// [`CatalogError::SchemaTooNew`]; a file whose tables differ from
    /// the compiled-in specs with [`CatalogError::Verify`].
    pub fn open(path: &Path) -> Result<Option<Catalog>> {
        match bookrack_dbkit::open_store(&STORE, path, Access::Read, None).map_err(refused(path))? {
            Opened::Ready(conn) => Ok(Some(Catalog::reading(conn))),
            Opened::Missing | Opened::Declined => Ok(None),
        }
    }

    /// Open the `catalog.db` at `path` read-only, requiring the file.
    ///
    /// The checks of [`Catalog::open`], with a missing file reported as
    /// the SQLite open failure it is rather than as `None`. Any write
    /// through the resulting handle is rejected by SQLite.
    pub fn open_read_only(path: &Path) -> Result<Catalog> {
        Ok(Catalog::reading(
            bookrack_dbkit::open_existing(&STORE, path).map_err(refused(path))?,
        ))
    }

    /// Open the `catalog.db` at `path` for writing, but only when its
    /// schema is already at this build's revision.
    ///
    /// `Ok(None)` means the database is not one this call may write:
    /// either no file is there, or the file is behind and a write door
    /// would migrate it. Migration is forward-only, so a caller whose
    /// write is incidental to its real work — a preview recording that
    /// it ran, a probe noting its own pass — has no business triggering
    /// one; it takes the `None` and does without the row. A database
    /// newer than this build is still an error, not a `None`: that one
    /// no caller can proceed on.
    pub fn open_for_bookkeeping(path: &Path) -> Result<Option<Catalog>> {
        match bookrack_dbkit::open_store(&STORE, path, Access::Bookkeeping, None)
            .map_err(refused(path))?
        {
            Opened::Ready(conn) => Catalog::writing(conn).map(Some),
            Opened::Missing | Opened::Declined => Ok(None),
        }
    }

    /// Open the `catalog.db` at `path` for writing, creating it if it
    /// does not exist and backing it up into `backup_dir` before
    /// applying any pending migration.
    ///
    /// Only an existing database that holds tables and is actually about
    /// to be migrated is backed up; a freshly created one is not. Fails
    /// with [`CatalogError::SchemaTooNew`] if the file was written by a
    /// newer schema revision than this build understands, and with
    /// [`CatalogError::RetiredLabels`], before the backup, if a file
    /// behind this build holds rows carrying retired labels.
    pub fn open_for_write(path: &Path, backup_dir: &Path) -> Result<Catalog> {
        Catalog::ready(
            bookrack_dbkit::open_store(&STORE, path, Access::Write, Some(backup_dir))
                .map_err(refused(path))?,
        )
    }

    /// Open the `catalog.db` at `path` for writing, creating it if it
    /// does not exist, without taking a pre-migration backup.
    ///
    /// For tests that build a database: a migration through this door
    /// leaves no snapshot behind. Production code takes
    /// [`Catalog::open_for_write`], which snapshots an existing database
    /// before migrating it.
    #[doc(hidden)]
    pub fn open_for_write_unbacked(path: &Path) -> Result<Catalog> {
        Catalog::ready(
            bookrack_dbkit::open_store(&STORE, path, Access::Write, None).map_err(refused(path))?,
        )
    }

    /// Open an ephemeral, private `catalog.db` held entirely in memory.
    /// The database vanishes when the handle is dropped.
    pub fn open_in_memory() -> Result<Catalog> {
        Catalog::writing(bookrack_dbkit::open_in_memory(&STORE)?)
    }

    fn ready(opened: Opened) -> Result<Catalog> {
        match opened {
            Opened::Ready(conn) => Catalog::writing(conn),
            Opened::Missing | Opened::Declined => {
                unreachable!("a write door creates and migrates")
            }
        }
    }

    fn reading(conn: Connection) -> Catalog {
        Catalog {
            conn: TimedConnection::new(conn, STORE.label),
            read_only: true,
        }
    }

    /// Wrap a writable connection and mirror the schema version into
    /// `catalog_meta`, for audit and for any tool still reading the
    /// stamp. `user_version` stays the authority.
    fn writing(conn: Connection) -> Result<Catalog> {
        let catalog = Catalog {
            conn: TimedConnection::new(conn, STORE.label),
            read_only: false,
        };
        catalog.meta_set(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_string())?;
        Ok(catalog)
    }

    /// Whether this handle was opened read-only.
    ///
    /// `true` for handles produced by [`Catalog::open`] and
    /// [`Catalog::open_read_only`]. Diagnostic — the actual write
    /// barrier is the SQLite `query_only` PRAGMA, not this flag.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The current UTC time as an ISO-8601 string, read from SQLite's
    /// clock so timestamps the crate writes share one source with the
    /// `ts` columns the table inserts generate themselves.
    pub fn now_iso(&self) -> Result<String> {
        Ok(bookrack_dbkit::now_iso(&self.conn)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_core::ItemKind;
    use bookrack_dbkit::READER_VERSION;

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
            let catalog = Catalog::open_for_write_unbacked(&path).expect("first open");
            catalog
                .meta_set(MIN_READER_VERSION_KEY, &too_new.to_string())
                .expect("overwrite stamp with a too-new value");
        }

        let Err(err) = Catalog::open_for_write_unbacked(&path) else {
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
    fn open_if_current_writes_a_current_database_and_declines_the_rest() {
        let dir = tempfile::tempdir().expect("temp dir");

        // Absent: nothing to open, and nothing created by asking.
        let absent = dir.path().join("absent.db");
        assert!(
            Catalog::open_for_bookkeeping(&absent)
                .expect("an absent file is not an error")
                .is_none()
        );
        assert!(!absent.exists(), "asking must not create the file");

        // Behind: an empty file reads as revision 0, which `open` would
        // migrate forward. The handle is declined and the recorded
        // revision does not move. `user_version` is the witness rather
        // than the file size: opening a connection at all writes a page
        // header, so bytes cannot tell a declined open from a migrated
        // one.
        let behind = dir.path().join("behind.db");
        std::fs::write(&behind, b"").expect("seed an uninitialised file");
        assert!(
            Catalog::open_for_bookkeeping(&behind)
                .expect("a database behind is not an error")
                .is_none()
        );
        let version: i64 = Connection::open(&behind)
            .expect("reopen the declined file")
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 0, "declining must not migrate");

        // Current: opens, and the handle can actually write.
        let current = dir.path().join("current.db");
        drop(Catalog::open_for_write_unbacked(&current).expect("build a current database"));
        let catalog = Catalog::open_for_bookkeeping(&current)
            .expect("a current database opens")
            .expect("a current database yields a handle");
        assert!(!catalog.is_read_only());
    }

    #[test]
    fn opening_is_idempotent() {
        // Re-running the schema batch against an initialized database
        // must neither fail nor disturb the recorded version. This needs
        // a real file, since each in-memory database is distinct.
        let dir = std::env::temp_dir().join(format!("bookrack-catalog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("catalog.db");

        Catalog::open_for_write_unbacked(&path).expect("first open");
        // Scope the reopened handle so its connection is closed before
        // the cleanup: Windows refuses to delete a file still held open.
        let version = {
            let reopened = Catalog::open_for_write_unbacked(&path).expect("second open");
            reopened.meta_get(SCHEMA_VERSION_KEY).expect("read")
        };
        assert_eq!(version, Some(SCHEMA_VERSION.to_string()));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn reopening_a_current_version_database_enforces_foreign_keys() {
        // The second open takes the no-migration branch; enforcement must
        // be on regardless of the linked SQLite's compile-time default.
        let dir = unique_dir("fk-steady-state");
        let path = dir.join("catalog.db");
        Catalog::open_for_write_unbacked(&path).expect("first open");
        {
            let reopened = Catalog::open_for_write_unbacked(&path).expect("second open");
            let fk: i64 = reopened
                .conn
                .pragma_query_value(None, "foreign_keys", |row| row.get(0))
                .expect("read pragma");
            assert_eq!(fk, 1, "foreign_keys must be on for a current-version open");
        }

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_newer_schema_version_is_refused() {
        // A database whose user_version exceeds the highest migration this
        // binary defines must be refused, not downgraded.
        let dir = unique_dir("refuse-newer");
        let path = dir.join("catalog.db");
        {
            let catalog = Catalog::open_for_write_unbacked(&path).expect("first open");
            catalog
                .conn
                .pragma_update(None, "user_version", TARGET_VERSION + 1)
                .expect("bump user_version");
        }
        let Err(err) = Catalog::open_for_write_unbacked(&path) else {
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
                Catalog::open_for_write(&path, &backup_dir).expect("adopt existing database");
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
        Catalog::open_for_write(&path, &backup_dir).expect("fresh open");
        let empty = !backup_dir.exists()
            || std::fs::read_dir(&backup_dir)
                .expect("read backup dir")
                .next()
                .is_none();
        assert!(empty, "a fresh database must not be backed up");

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
    fn open_read_only_refuses_a_missing_file_without_creating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");

        let Err(err) = Catalog::open_read_only(&path) else {
            panic!("missing file must refuse to open")
        };
        assert!(
            matches!(err, CatalogError::Sqlite(_)),
            "expected a SQLite open failure, got {err:?}"
        );
        assert!(!path.exists(), "read-only open must not create catalog.db");
    }

    #[test]
    fn open_reports_a_missing_file_as_absent_without_creating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");

        let opened = Catalog::open(&path).expect("a missing file is not an error");
        assert!(opened.is_none(), "missing file must probe as absent");
        assert!(!path.exists(), "the probe must not create catalog.db");
    }

    #[test]
    fn open_opens_an_existing_file_read_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        drop(Catalog::open_for_write_unbacked(&path).expect("seed"));

        let opened = Catalog::open(&path).expect("open");
        assert!(opened.is_some_and(|c| c.is_read_only()));
    }

    #[test]
    fn open_still_refuses_an_unmigrated_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        std::fs::write(&path, b"").expect("seed an uninitialised file");

        let Err(err) = Catalog::open(&path) else {
            panic!("an unmigrated file must report its reason, not probe as absent")
        };
        assert!(
            matches!(err, CatalogError::SchemaTooOld { found: 0, expected } if expected == TARGET_VERSION),
            "{err:?}"
        );
    }

    #[test]
    fn open_reports_a_directory_in_the_files_place_as_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        std::fs::create_dir(&path).expect("a directory where the file belongs");

        assert!(
            Catalog::open(&path).is_err(),
            "a directory is not a missing catalog"
        );
    }

    #[test]
    fn open_refuses_a_current_catalog_whose_tables_differ_from_the_specs() {
        // `user_version` says current, one column says otherwise: only
        // the conformance check can tell.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        {
            let catalog = Catalog::open_for_write_unbacked(&path).expect("create");
            catalog
                .conn
                .execute_batch("ALTER TABLE intake RENAME COLUMN source_sha256 TO renamed;")
                .expect("rename a column");
        }

        let Err(err) = Catalog::open(&path) else {
            panic!("a drifted catalog must be refused")
        };
        assert!(matches!(err, CatalogError::Verify(_)), "{err:?}");
    }

    #[test]
    fn open_read_only_rejects_writes() {
        use crate::{IntakeStatus, NewIntake};

        let dir = unique_dir("read-only-blocks-writes");
        let path = dir.join("catalog.db");
        {
            // Initialize the schema through the read-write entry point.
            let mut catalog = Catalog::open_for_write_unbacked(&path).expect("first open");
            catalog
                .register_intake(ItemKind::Book, &NewIntake::new("sha-rw"))
                .expect("seed");
        }

        let read_only = Catalog::open_read_only(&path).expect("open read-only");
        assert!(read_only.is_read_only());

        // The existing row is still readable.
        let by_sha = read_only
            .intake_by_sha(ItemKind::Book, "sha-rw")
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
        Catalog::open_for_write_unbacked(&path).expect("first open");
        let read_only = Catalog::open_read_only(&path).expect("read-only open");
        // The acceptance gate ran: the version mirror is still readable.
        let version = read_only.meta_get(SCHEMA_VERSION_KEY).expect("read");
        assert_eq!(version, Some(SCHEMA_VERSION.to_string()));
        assert!(read_only.is_read_only());

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn open_read_only_refuses_an_unmigrated_older_schema() {
        // A file stopped at an early revision: the read-only door names
        // the revision it found and never migrates forward.
        let dir = unique_dir("read-only-older");
        let path = dir.join("catalog.db");
        {
            let mut conn = Connection::open(&path).expect("seed connection");
            migrations()
                .to_version(&mut conn, 2)
                .expect("stop at an early revision");
        }

        let Err(err) = Catalog::open_read_only(&path) else {
            panic!("an old-revision file must be refused")
        };
        assert!(
            matches!(err, CatalogError::SchemaTooOld { found: 2, expected } if expected == TARGET_VERSION),
            "{err:?}"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn open_read_only_refuses_a_newer_schema_version() {
        let dir = unique_dir("read-only-newer");
        let path = dir.join("catalog.db");
        {
            let catalog = Catalog::open_for_write_unbacked(&path).expect("create");
            catalog
                .conn
                .pragma_update(None, "user_version", TARGET_VERSION + 1)
                .expect("bump user_version");
        }

        let Err(err) = Catalog::open_read_only(&path) else {
            panic!("a newer-revision file must be refused")
        };
        assert!(
            matches!(err, CatalogError::SchemaTooNew { found, expected }
                if found == TARGET_VERSION + 1 && expected == TARGET_VERSION),
            "{err:?}"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn open_read_only_accepts_a_missing_reader_stamp_without_seeding_it() {
        let dir = unique_dir("read-only-unseeded");
        let path = dir.join("catalog.db");
        {
            let catalog = Catalog::open_for_write_unbacked(&path).expect("create");
            catalog
                .conn
                .execute(
                    &format!("DELETE FROM catalog_meta WHERE key = '{MIN_READER_VERSION_KEY}'"),
                    [],
                )
                .expect("remove the reader stamp");
        }

        // A missing stamp resolves to Match, and the read-only contract
        // forbids writing the seed the read-write path would take.
        {
            let read_only = Catalog::open_read_only(&path).expect("open without a stamp");
            assert_eq!(
                read_only.meta_get(MIN_READER_VERSION_KEY).expect("read"),
                None,
                "a read-only open must not seed the stamp"
            );
        }
        // The next read-write open seeds it again.
        {
            let reopened = Catalog::open_for_write_unbacked(&path).expect("read-write reopen");
            assert_eq!(
                reopened.meta_get(MIN_READER_VERSION_KEY).expect("read"),
                Some(MIN_READER_VERSION.to_string())
            );
        }

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

    /// A read-only stats-shaped scan drains through a writer's
    /// `BEGIN EXCLUSIVE` hold. The production helpers put the
    /// catalog in WAL with a busy timeout, so the reader sees a
    /// committed snapshot and the EXCLUSIVE-window upgrade no
    /// longer surfaces as `catalog error` to consumers like
    /// `library.stats`.
    #[test]
    fn open_read_only_drains_through_a_writer_exclusive_hold() {
        use crate::NewIntake;
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let dir = unique_dir("read-during-exclusive");
        let path = dir.join("catalog.db");
        {
            let mut catalog = Catalog::open_for_write_unbacked(&path).expect("seed open");
            for i in 0..32 {
                catalog
                    .register_intake(ItemKind::Book, &NewIntake::new(format!("sha-{i}")))
                    .expect("seed row");
            }
        }

        let writer_path = path.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let conn = bookrack_dbkit::open_production(&writer_path).expect("writer open");
            conn.execute_batch("BEGIN EXCLUSIVE;").expect("begin");
            ready_tx.send(()).expect("notify ready");
            thread::sleep(Duration::from_millis(1_500));
            conn.execute_batch("COMMIT;").expect("commit");
        });

        ready_rx.recv().expect("writer ready");

        let start = Instant::now();
        let reader = Catalog::open_read_only(&path).expect("reader open");
        let count: i64 = reader
            .conn
            .query_row("SELECT count(*) FROM intake", [], |row| row.get(0))
            .expect("stats scan");
        let elapsed = start.elapsed();

        writer.join().expect("writer join");
        assert_eq!(count, 32);
        assert!(
            elapsed < Duration::from_secs(5),
            "stats scan took {elapsed:?}; reader must drain inside busy timeout",
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// A unique temp directory for a test that needs a real file, tagged so
    /// parallel tests do not collide.
    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bookrack-catalog-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }
    /// A file at `path` one revision behind, holding one intake row per
    /// `sha`, with the last one then removed so the AUTOINCREMENT
    /// counter sits above every surviving id.
    fn seed_behind(path: &Path, shas: &[&str]) {
        crate::fixtures::create_at_version(path, TARGET_VERSION - 1);
        let conn = Connection::open(path).expect("open fixture");
        for sha in shas {
            conn.execute(
                "INSERT INTO intake (source_sha256, intake_at, status) \
                 VALUES (?1, '2026-01-01T00:00:00Z', 'embedded')",
                [sha],
            )
            .expect("seed row");
        }
        conn.execute(
            "DELETE FROM intake WHERE source_sha256 = ?1",
            [shas[shas.len() - 1]],
        )
        .expect("remove the last row");
    }

    fn kinds(path: &Path) -> Vec<(String, i64)> {
        let conn = Connection::open(path).expect("reopen");
        let mut stmt = conn
            .prepare("SELECT kind, COUNT(*) FROM intake GROUP BY kind ORDER BY kind")
            .expect("prepare");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows")
    }

    #[test]
    fn migrating_labels_the_rows_of_a_papers_catalog_paper_by_its_file_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let papers = dir.path().join(crate::PAPERS_CATALOG_FILE);
        let books = dir.path().join("catalog.db");
        seed_behind(&papers, &["p1", "p2", "p3"]);
        seed_behind(&books, &["b1", "b2", "b3"]);
        // The write door itself, with no paper-side code in between.
        drop(Catalog::open_for_write_unbacked(&papers).expect("open papers catalog"));
        drop(Catalog::open_for_write_unbacked(&books).expect("open books catalog"));
        assert_eq!(kinds(&papers), vec![("paper".to_string(), 2)]);
        assert_eq!(kinds(&books), vec![("book".to_string(), 2)]);

        let papers = Catalog::open_read_only(&papers).expect("reopen papers");
        assert_eq!(papers.count_intakes(ItemKind::Paper).expect("count"), 2);
        assert_eq!(papers.count_intakes(ItemKind::Book).expect("count"), 0);
        assert!(
            papers
                .intake_by_sha(ItemKind::Paper, "p1")
                .expect("lookup")
                .is_some()
        );
        assert!(
            papers
                .intake_by_sha(ItemKind::Book, "p1")
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn the_doors_that_do_not_migrate_leave_a_behind_papers_catalog_unlabelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let papers = dir.path().join(crate::PAPERS_CATALOG_FILE);
        seed_behind(&papers, &["p1", "p2"]);

        assert!(matches!(
            Catalog::open(&papers),
            Err(CatalogError::SchemaTooOld { .. })
        ));
        assert!(
            Catalog::open_for_bookkeeping(&papers)
                .expect("declined")
                .is_none()
        );
        assert_eq!(
            crate::fixtures::user_version(&papers),
            TARGET_VERSION - 1,
            "neither door migrates"
        );
        let conn = Connection::open(&papers).expect("reopen");
        let has_kind: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('intake') WHERE name = 'kind'",
                [],
                |row| row.get(0),
            )
            .expect("columns");
        assert_eq!(has_kind, 0);
    }

    #[test]
    fn migrating_keeps_the_autoincrement_counter_above_a_removed_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        seed_behind(&path, &["a", "b", "c"]);
        let mut catalog = Catalog::open_for_write_unbacked(&path).expect("open");
        let fresh = catalog
            .register_intake(ItemKind::Book, &crate::NewIntake::new("d"))
            .expect("register")
            .into_intake();
        assert_eq!(
            fresh.intake_id, 4,
            "id 3 was issued before and must not return"
        );
    }

    #[test]
    fn the_same_bytes_hold_one_row_per_kind_and_never_two_of_one_kind() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let book = catalog
            .register_intake(ItemKind::Book, &crate::NewIntake::new("same"))
            .expect("book");
        let reference = catalog
            .register_intake(ItemKind::Reference, &crate::NewIntake::new("same"))
            .expect("reference");
        assert!(book.is_new() && reference.is_new());
        assert_ne!(book.intake().intake_id, reference.intake().intake_id);

        let again = catalog
            .register_intake(ItemKind::Book, &crate::NewIntake::new("same"))
            .expect("book again");
        assert!(!again.is_new());
        assert_eq!(again.intake().intake_id, book.intake().intake_id);
        assert_eq!(
            catalog
                .intake_by_sha(ItemKind::Book, "same")
                .expect("lookup")
                .expect("present")
                .intake_id,
            book.intake().intake_id
        );
        assert_eq!(catalog.count_intakes(ItemKind::Book).expect("count"), 1);
        assert_eq!(
            catalog.count_intakes(ItemKind::Reference).expect("count"),
            1
        );

        let dup = catalog.conn.execute(
            "INSERT INTO intake (source_sha256, intake_at, status, kind) \
             VALUES ('same', '2026-01-01T00:00:00Z', 'pending', 'book')",
            [],
        );
        assert!(dup.is_err(), "a second (book, same) row must be refused");
    }

    #[test]
    fn a_rule_fingerprint_is_absent_until_set_and_reads_back() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let id = catalog
            .register_intake(ItemKind::Book, &crate::NewIntake::new("ruled"))
            .expect("register")
            .into_intake()
            .intake_id;
        let read = |catalog: &Catalog| {
            catalog
                .intake_by_id(id)
                .expect("lookup")
                .expect("present")
                .rule_fingerprint
        };
        assert_eq!(read(&catalog), None);
        assert!(catalog.set_rule_fingerprint(id, "fp-1").expect("set"));
        assert_eq!(read(&catalog).as_deref(), Some("fp-1"));
        assert!(catalog.set_rule_fingerprint(id, "fp-2").expect("replace"));
        assert_eq!(read(&catalog).as_deref(), Some("fp-2"));
        assert!(!catalog.set_rule_fingerprint(id + 1, "fp").expect("no row"));
    }
}
