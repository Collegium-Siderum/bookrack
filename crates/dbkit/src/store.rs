// SPDX-License-Identifier: Apache-2.0

//! The one open sequence every SQLite store goes through.
//!
//! A store describes itself once, as a [`StoreSpec`]: its tables, how
//! its schema is versioned, and where it keeps its reader stamp. A
//! caller says what it means to do, as an [`Access`]. [`open_store`]
//! then inspects the file and either hands back a connection, says
//! there is nothing to open, or refuses with the reason.
//!
//! What each access gets for each state of the file:
//!
//! | on disk | `Read` | `Bookkeeping` | `Write` |
//! |---|---|---|---|
//! | no file | `Missing` | `Missing` | created, `Ready` |
//! | current | `Ready`, read-only | `Ready` | `Ready` |
//! | behind, migratable | `TooOld` | `Declined` | backed up, migrated, `Ready` |
//! | stamped with another revision | `Rederive` | unsupported | `Rederive` |
//! | newer than this build | `TooNew` | `TooNew` | `TooNew` |
//! | reader stamp above this build | `ReaderTooOld` | `ReaderTooOld` | `ReaderTooOld` |
//! | tables differ from the spec | `Verify` | `Verify` | `Verify` |
//!
//! `Read` never creates a file, never migrates, and never writes a
//! stamp. `Bookkeeping` writes into a store that is already current and
//! declines every other one, so a write incidental to the caller's real
//! work never migrates anything. `Write` is the only access that creates
//! or migrates.
//!
//! A refusal under `Bookkeeping` or `Write` means nothing was migrated,
//! no table was created, and no stamp was written. It does not mean the
//! file's bytes are untouched: a writable connection sets the journal
//! mode as it opens.

use std::path::Path;

use rusqlite::Connection;

use crate::backup::{backup_before_migration, has_user_tables};
use crate::connection::{open_production, open_production_strict_read_only};
use crate::meta::{apply_schema, meta_get, meta_set};
use crate::open::READER_VERSION;
use crate::spec::TableSpec;
use crate::verify::{VerifyError, verify_all};

/// What a caller may do through the connection it asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Look. The file is never created, migrated, or stamped.
    Read,
    /// Write into a store that is already current; decline any other.
    Bookkeeping,
    /// Create the store if it is missing and migrate it if it is behind.
    Write,
}

/// One store's side of the open protocol.
#[derive(Debug)]
pub struct StoreSpec {
    /// Names the store in a backup file when the path has no stem.
    pub label: &'static str,
    /// The tables the store holds, verified on every open.
    pub tables: &'static [&'static TableSpec],
    /// How the store's schema is versioned.
    pub schema: SchemaAxis,
    /// Where the store keeps its `min_reader_version`.
    pub reader_stamp: ReaderStamp,
}

/// The key/value meta table of a store and the reader stamp in it.
#[derive(Debug)]
pub struct ReaderStamp {
    /// The meta table's name.
    pub table: &'static str,
    /// The key the stamp is kept under.
    pub key: &'static str,
    /// The value a writable open records when the stamp is absent.
    pub write_version: u32,
}

/// How a store's schema is versioned.
#[derive(Debug)]
pub enum SchemaAxis {
    /// Forward-only migrations keyed by `user_version`.
    Migrated {
        /// The `user_version` this build reads and writes.
        target: i64,
        /// Brings a connection at any earlier version to `target`.
        migrate: fn(&mut Connection) -> Result<(), rusqlite_migration::Error>,
    },
    /// A derived store stamped with one revision. A store stamped with
    /// another is rebuilt from its sources, never migrated.
    Stamped {
        /// The key in [`ReaderStamp::table`] the revision is kept under.
        key: &'static str,
        /// The revision this build reads and writes.
        value: &'static str,
    },
}

/// The outcome of an open that did not fail.
#[derive(Debug)]
pub enum Opened {
    /// No file is there, and the access does not create one.
    Missing,
    /// The store is behind, and the access does not migrate.
    Declined,
    /// The store is current and the connection is ready.
    Ready(Connection),
}

/// Why a store could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The store is behind this build and the access does not migrate.
    #[error("schema version {found} is behind this build's {target}")]
    TooOld {
        /// The `user_version` on disk.
        found: i64,
        /// The `user_version` this build reads.
        target: i64,
    },
    /// The store was written by a newer build.
    #[error("schema version {found} is newer than this build's {target}")]
    TooNew {
        /// The `user_version` on disk.
        found: i64,
        /// The `user_version` this build reads.
        target: i64,
    },
    /// The store requires a newer reader than this build.
    #[error("the store requires reader version {required}; this build is {current}")]
    ReaderTooOld {
        /// The `min_reader_version` on disk.
        required: u32,
        /// This build's [`READER_VERSION`].
        current: u32,
    },
    /// The store is stamped with another revision and must be rebuilt.
    #[error("the store is stamped {found:?}; this build reads {expected:?}")]
    Rederive {
        /// The revision on disk.
        found: String,
        /// The revision this build reads.
        expected: &'static str,
    },
    /// The live tables differ from the spec.
    #[error(transparent)]
    Verify(VerifyError),
    /// A migration failed.
    #[error(transparent)]
    Migrate(rusqlite_migration::Error),
    /// SQLite failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// The pre-migration backup could not be written or pruned.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Open the store at `path` for `access`.
///
/// A file counts as missing only when the filesystem reports it not
/// found. Anything else in its place, such as a directory of the same
/// name, is left for the open itself to fail on.
///
/// `backup_dir` matters to a migrating `Write` only: a store that holds
/// tables is snapshotted there before its first migration runs. The
/// snapshot is named after the file's stem, so two stores that share
/// one spec and one directory keep their snapshots apart.
///
/// # Panics
///
/// Panics when `access` is [`Access::Bookkeeping`] and the store is
/// [`SchemaAxis::Stamped`]: no such store has a bookkeeping caller, and
/// the order of its checks is not defined.
pub fn open_store(
    spec: &StoreSpec,
    path: &Path,
    access: Access,
    backup_dir: Option<&Path>,
) -> Result<Opened, OpenError> {
    refuse_stamped_bookkeeping(spec, access);
    let missing =
        matches!(std::fs::metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    if missing && access != Access::Write {
        return Ok(Opened::Missing);
    }
    let conn = match access {
        Access::Read => open_production_strict_read_only(path)?,
        Access::Bookkeeping | Access::Write => open_production(path)?,
    };
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(spec.label);
    admit(spec, conn, access, backup_dir.map(|dir| (dir, stem)))
}

/// Open the store at `path` read-only, requiring the file.
///
/// The same checks as [`open_store`] under [`Access::Read`], without the
/// probe for a missing file: a missing file is the failure of the
/// underlying open, reported as [`OpenError::Sqlite`].
pub fn open_existing(spec: &StoreSpec, path: &Path) -> Result<Connection, OpenError> {
    let conn = open_production_strict_read_only(path)?;
    match admit(spec, conn, Access::Read, None)? {
        Opened::Ready(conn) => Ok(conn),
        Opened::Missing | Opened::Declined => {
            unreachable!("a read of an open connection is ready or refused")
        }
    }
}

/// Create the store in a private in-memory database.
pub fn open_in_memory(spec: &StoreSpec) -> Result<Connection, OpenError> {
    let conn = Connection::open_in_memory()?;
    match admit(spec, conn, Access::Write, None)? {
        Opened::Ready(conn) => Ok(conn),
        Opened::Missing | Opened::Declined => unreachable!("a write is ready or refused"),
    }
}

fn refuse_stamped_bookkeeping(spec: &StoreSpec, access: Access) {
    assert!(
        !(access == Access::Bookkeeping && matches!(spec.schema, SchemaAxis::Stamped { .. })),
        "a stamped store has no bookkeeping access",
    );
}

/// Run the checks of `access` on an open connection.
fn admit(
    spec: &StoreSpec,
    conn: Connection,
    access: Access,
    backup: Option<(&Path, &str)>,
) -> Result<Opened, OpenError> {
    refuse_stamped_bookkeeping(spec, access);
    match spec.schema {
        SchemaAxis::Migrated { target, migrate } => {
            admit_migrated(spec, conn, access, backup, target, migrate)
        }
        SchemaAxis::Stamped { key, value } => admit_stamped(spec, conn, access, key, value),
    }
}

/// `user_version` first, then the table shapes, then the reader stamp.
///
/// The version comes first so that an empty or behind file reports its
/// version rather than a table it lacks. The reader stamp comes last
/// because it lives in a table the shape check has just vouched for.
fn admit_migrated(
    spec: &StoreSpec,
    mut conn: Connection,
    access: Access,
    backup: Option<(&Path, &str)>,
    target: i64,
    migrate: fn(&mut Connection) -> Result<(), rusqlite_migration::Error>,
) -> Result<Opened, OpenError> {
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > target {
        return Err(OpenError::TooNew { found, target });
    }
    if found < target {
        match access {
            Access::Read => return Err(OpenError::TooOld { found, target }),
            Access::Bookkeeping => return Ok(Opened::Declined),
            Access::Write => {
                if let Some((dir, stem)) = backup
                    && has_user_tables(&conn)?
                {
                    backup_before_migration(&conn, dir, stem, found)?;
                }
                // A table rebuild needs foreign keys off, and the pragma
                // is a no-op inside the migration's own transaction.
                conn.pragma_update(None, "foreign_keys", "OFF")?;
                migrate(&mut conn).map_err(OpenError::Migrate)?;
            }
        }
    }
    if access != Access::Read {
        enforce_foreign_keys(&conn)?;
    }
    verify_all(&conn, spec.tables).map_err(OpenError::Verify)?;
    check_reader_stamp(&conn, &spec.reader_stamp)?;
    if access != Access::Read {
        seed_reader_stamp(&conn, &spec.reader_stamp)?;
    }
    Ok(Opened::Ready(conn))
}

/// Both stamps first, then the tables.
///
/// A store left by an older build often has an old stamp and old table
/// shapes at once. Comparing the stamps before anything else reports
/// the cause that has a remedy, and under `Write` it keeps this build's
/// tables and indexes out of a store that is about to be refused.
fn admit_stamped(
    spec: &StoreSpec,
    conn: Connection,
    access: Access,
    key: &'static str,
    value: &'static str,
) -> Result<Opened, OpenError> {
    let stamp = &spec.reader_stamp;
    if has_table(&conn, stamp.table)? {
        if let Some(found) = meta_get(&conn, stamp.table, key)?
            && found != value
        {
            return Err(OpenError::Rederive {
                found,
                expected: value,
            });
        }
        check_reader_stamp(&conn, stamp)?;
    }
    if access == Access::Read {
        verify_all(&conn, spec.tables).map_err(OpenError::Verify)?;
        return Ok(Opened::Ready(conn));
    }
    enforce_foreign_keys(&conn)?;
    apply_schema(&conn, spec.tables)?;
    if meta_get(&conn, stamp.table, key)?.is_none() {
        meta_set(&conn, stamp.table, key, value)?;
    }
    verify_all(&conn, spec.tables).map_err(OpenError::Verify)?;
    seed_reader_stamp(&conn, stamp)?;
    Ok(Opened::Ready(conn))
}

/// Turn foreign-key enforcement on for this connection.
///
/// The setting is per connection, and its default is a compile-time
/// option of the linked SQLite, so a writable connection states it.
fn enforce_foreign_keys(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")
}

fn has_table(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
}

fn read_reader_stamp(conn: &Connection, stamp: &ReaderStamp) -> rusqlite::Result<Option<u32>> {
    Ok(meta_get(conn, stamp.table, stamp.key)?.and_then(|s| s.parse::<u32>().ok()))
}

/// Refuse a reader stamp above this build. An absent stamp passes.
fn check_reader_stamp(conn: &Connection, stamp: &ReaderStamp) -> Result<(), OpenError> {
    match read_reader_stamp(conn, stamp)? {
        Some(required) if required > READER_VERSION => Err(OpenError::ReaderTooOld {
            required,
            current: READER_VERSION,
        }),
        _ => Ok(()),
    }
}

/// Record the reader stamp when the store has none.
fn seed_reader_stamp(conn: &Connection, stamp: &ReaderStamp) -> rusqlite::Result<()> {
    if read_reader_stamp(conn, stamp)?.is_none() {
        meta_set(
            conn,
            stamp.table,
            stamp.key,
            &stamp.write_version.to_string(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use rusqlite_migration::{M, Migrations};
    use tempfile::{TempDir, tempdir};

    use crate::ddl::render_ddl;
    use crate::spec::{ColumnSpec, IndexSpec};

    const META: TableSpec = TableSpec {
        name: "meta",
        comment: None,
        columns: &[
            ColumnSpec::text("key").primary_key(),
            ColumnSpec::text("value").not_null(),
        ],
        composite_pk: None,
        uniques: &[],
        table_checks: &[],
        indexes: &[],
    };
    const ITEM: TableSpec = TableSpec {
        name: "item",
        comment: None,
        columns: &[ColumnSpec::int("id").primary_key()],
        composite_pk: None,
        uniques: &[],
        table_checks: &[],
        indexes: &[],
    };
    /// `item` as a later build declares it: one more column, indexed.
    const ITEM_NEXT: TableSpec = TableSpec {
        name: "item",
        comment: None,
        columns: &[
            ColumnSpec::int("id").primary_key(),
            ColumnSpec::text("label"),
        ],
        composite_pk: None,
        uniques: &[],
        table_checks: &[],
        indexes: &[IndexSpec::on("idx_item_label", &["label"])],
    };
    const EXTRA: TableSpec = TableSpec {
        name: "extra",
        comment: None,
        columns: &[ColumnSpec::int("id").primary_key()],
        composite_pk: None,
        uniques: &[],
        table_checks: &[],
        indexes: &[],
    };
    const STAMP: ReaderStamp = ReaderStamp {
        table: "meta",
        key: "min_reader_version",
        write_version: 1,
    };

    /// Two migrations: the first creates `meta`, the second `item`.
    const TARGET: i64 = 2;

    fn steps() -> Migrations<'static> {
        Migrations::new(vec![
            M::up("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);"),
            M::up("CREATE TABLE item (id INTEGER PRIMARY KEY);"),
        ])
    }

    fn to_latest(conn: &mut Connection) -> Result<(), rusqlite_migration::Error> {
        steps().to_latest(conn)
    }

    const MIGRATED: StoreSpec = StoreSpec {
        label: "store",
        tables: &[&META, &ITEM],
        schema: SchemaAxis::Migrated {
            target: TARGET,
            migrate: to_latest,
        },
        reader_stamp: STAMP,
    };
    const STAMPED: StoreSpec = StoreSpec {
        label: "store",
        tables: &[&META, &ITEM],
        schema: SchemaAxis::Stamped {
            key: "schema_version",
            value: "1",
        },
        reader_stamp: STAMP,
    };
    /// The stamped store as a later build declares it.
    const STAMPED_NEXT: StoreSpec = StoreSpec {
        label: "store",
        tables: &[&META, &ITEM_NEXT, &EXTRA],
        schema: SchemaAxis::Stamped {
            key: "schema_version",
            value: "2",
        },
        reader_stamp: STAMP,
    };

    const ACCESSES: [Access; 3] = [Access::Read, Access::Bookkeeping, Access::Write];
    const STAMPED_ACCESSES: [Access; 2] = [Access::Read, Access::Write];

    struct Disk {
        dir: TempDir,
    }

    impl Disk {
        fn new() -> Disk {
            Disk {
                dir: tempdir().expect("tempdir"),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("store.db")
        }

        fn backups(&self) -> PathBuf {
            self.dir.path().join("backups")
        }

        /// A store of `spec` at its current revision.
        fn current(spec: &StoreSpec) -> Disk {
            let disk = Disk::new();
            ready(open_store(spec, &disk.path(), Access::Write, None));
            disk
        }

        /// A migrated store stopped one revision short, holding a table.
        fn behind() -> Disk {
            let disk = Disk::new();
            let mut conn = Connection::open(disk.path()).expect("seed");
            steps().to_version(&mut conn, 1).expect("stop at v1");
            disk
        }

        fn raw(&self) -> Connection {
            Connection::open(self.path()).expect("raw connection")
        }

        fn user_version(&self) -> i64 {
            self.raw()
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("user_version")
        }

        fn schema_rows(&self) -> Vec<String> {
            let conn = self.raw();
            let mut stmt = conn
                .prepare("SELECT type || ' ' || name FROM sqlite_master ORDER BY 1")
                .expect("prepare");
            stmt.query_map([], |row| row.get(0))
                .expect("query")
                .map(|r| r.expect("row"))
                .collect()
        }

        fn backup_names(&self) -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(self.backups()) else {
                return Vec::new();
            };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    fn ready(opened: Result<Opened, OpenError>) -> Connection {
        match opened.expect("open") {
            Opened::Ready(conn) => conn,
            other => panic!("expected a ready connection, got {other:?}"),
        }
    }

    fn refusal(opened: Result<Opened, OpenError>) -> OpenError {
        match opened {
            Err(e) => e,
            Ok(other) => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn can_write(conn: &Connection) -> bool {
        conn.execute("INSERT INTO meta(key, value) VALUES('probe', 'x')", [])
            .is_ok()
    }

    // ── Migrated: no file ───────────────────────────────────────────

    #[test]
    fn migrated_read_of_no_file_is_missing_and_creates_nothing() {
        let disk = Disk::new();
        let opened = open_store(&MIGRATED, &disk.path(), Access::Read, None).expect("open");
        assert!(matches!(opened, Opened::Missing), "{opened:?}");
        assert!(!disk.path().exists());
    }

    #[test]
    fn migrated_bookkeeping_of_no_file_is_missing_and_creates_nothing() {
        let disk = Disk::new();
        let opened = open_store(&MIGRATED, &disk.path(), Access::Bookkeeping, None).expect("open");
        assert!(matches!(opened, Opened::Missing), "{opened:?}");
        assert!(!disk.path().exists());
    }

    #[test]
    fn migrated_write_of_no_file_creates_migrates_and_stamps() {
        let disk = Disk::new();
        let conn = ready(open_store(
            &MIGRATED,
            &disk.path(),
            Access::Write,
            Some(&disk.backups()),
        ));
        assert_eq!(disk.user_version(), TARGET);
        assert_eq!(read_reader_stamp(&conn, &STAMP).expect("stamp"), Some(1));
        assert!(
            disk.backup_names().is_empty(),
            "a new store has nothing to back up"
        );
    }

    // ── Migrated: current ───────────────────────────────────────────

    #[test]
    fn migrated_read_of_a_current_store_is_ready_and_read_only() {
        let disk = Disk::current(&MIGRATED);
        let conn = ready(open_store(&MIGRATED, &disk.path(), Access::Read, None));
        assert!(!can_write(&conn));
    }

    #[test]
    fn migrated_bookkeeping_of_a_current_store_is_ready_and_writable() {
        let disk = Disk::current(&MIGRATED);
        let conn = ready(open_store(
            &MIGRATED,
            &disk.path(),
            Access::Bookkeeping,
            None,
        ));
        assert!(can_write(&conn));
    }

    #[test]
    fn migrated_write_of_a_current_store_is_ready_and_takes_no_backup() {
        let disk = Disk::current(&MIGRATED);
        let conn = ready(open_store(
            &MIGRATED,
            &disk.path(),
            Access::Write,
            Some(&disk.backups()),
        ));
        assert!(can_write(&conn));
        assert!(disk.backup_names().is_empty());
    }

    // ── Migrated: behind ────────────────────────────────────────────

    #[test]
    fn migrated_read_of_a_behind_store_is_too_old_and_does_not_migrate() {
        let disk = Disk::behind();
        let err = refusal(open_store(&MIGRATED, &disk.path(), Access::Read, None));
        assert!(
            matches!(
                err,
                OpenError::TooOld {
                    found: 1,
                    target: TARGET
                }
            ),
            "{err:?}"
        );
        assert_eq!(disk.user_version(), 1);
    }

    #[test]
    fn migrated_bookkeeping_of_a_behind_store_declines_and_does_not_migrate() {
        let disk = Disk::behind();
        let opened = open_store(&MIGRATED, &disk.path(), Access::Bookkeeping, None).expect("open");
        assert!(matches!(opened, Opened::Declined), "{opened:?}");
        assert_eq!(disk.user_version(), 1);
        assert_eq!(
            disk.schema_rows(),
            ["index sqlite_autoindex_meta_1", "table meta"]
        );
    }

    #[test]
    fn migrated_write_of_a_behind_store_backs_up_then_migrates() {
        let disk = Disk::behind();
        ready(open_store(
            &MIGRATED,
            &disk.path(),
            Access::Write,
            Some(&disk.backups()),
        ));
        assert_eq!(disk.user_version(), TARGET);
        let names = disk.backup_names();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(
            names[0].starts_with("store-") && names[0].ends_with("-from-v1.bak"),
            "{names:?}"
        );
        // The snapshot is the store as it stood before the migration.
        let snapshot = Connection::open(disk.backups().join(&names[0])).expect("open backup");
        let version: i64 = snapshot
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version");
        assert_eq!(version, 1);
    }

    #[test]
    fn migrated_write_takes_no_backup_without_a_directory() {
        let disk = Disk::behind();
        ready(open_store(&MIGRATED, &disk.path(), Access::Write, None));
        assert_eq!(disk.user_version(), TARGET);
        assert!(disk.backup_names().is_empty());
        assert!(!disk.backups().exists());
    }

    #[test]
    fn migrated_write_takes_no_backup_of_a_file_without_tables() {
        let disk = Disk::new();
        std::fs::write(disk.path(), b"").expect("an empty file");
        ready(open_store(
            &MIGRATED,
            &disk.path(),
            Access::Write,
            Some(&disk.backups()),
        ));
        assert_eq!(disk.user_version(), TARGET);
        assert!(disk.backup_names().is_empty());
    }

    // ── Migrated: newer, reader stamp, table shapes ─────────────────

    #[test]
    fn migrated_store_newer_than_this_build_is_too_new_under_every_access() {
        for access in ACCESSES {
            let disk = Disk::current(&MIGRATED);
            disk.raw()
                .pragma_update(None, "user_version", TARGET + 1)
                .expect("bump");
            let err = refusal(open_store(&MIGRATED, &disk.path(), access, None));
            assert!(
                matches!(err, OpenError::TooNew { found, target } if found == TARGET + 1 && target == TARGET),
                "{access:?}: {err:?}"
            );
        }
    }

    #[test]
    fn migrated_store_demanding_a_newer_reader_is_refused_under_every_access() {
        for access in ACCESSES {
            let disk = Disk::current(&MIGRATED);
            let demanded = READER_VERSION + 1;
            meta_set(&disk.raw(), "meta", STAMP.key, &demanded.to_string()).expect("stamp");
            let err = refusal(open_store(&MIGRATED, &disk.path(), access, None));
            assert!(
                matches!(err, OpenError::ReaderTooOld { required, current }
                    if required == demanded && current == READER_VERSION),
                "{access:?}: {err:?}"
            );
        }
    }

    #[test]
    fn migrated_store_with_drifted_tables_fails_verification_under_every_access() {
        for access in ACCESSES {
            let disk = Disk::current(&MIGRATED);
            disk.raw()
                .execute_batch("ALTER TABLE item ADD COLUMN stray TEXT;")
                .expect("drift");
            let err = refusal(open_store(&MIGRATED, &disk.path(), access, None));
            assert!(matches!(err, OpenError::Verify(_)), "{access:?}: {err:?}");
        }
    }

    // ── Migrated: order of the checks ───────────────────────────────

    #[test]
    fn an_empty_file_declines_bookkeeping_and_is_too_old_to_read() {
        // The version is read before the tables are looked at, so a
        // file with no tables reports its version, not a missing table.
        let disk = Disk::new();
        std::fs::write(disk.path(), b"").expect("an empty file");

        let opened = open_store(&MIGRATED, &disk.path(), Access::Bookkeeping, None).expect("open");
        assert!(matches!(opened, Opened::Declined), "{opened:?}");
        assert_eq!(disk.user_version(), 0);

        let err = refusal(open_store(&MIGRATED, &disk.path(), Access::Read, None));
        assert!(
            matches!(
                err,
                OpenError::TooOld {
                    found: 0,
                    target: TARGET
                }
            ),
            "{err:?}"
        );
    }

    // ── Stamped ─────────────────────────────────────────────────────

    #[test]
    fn stamped_read_of_no_file_is_missing_and_creates_nothing() {
        let disk = Disk::new();
        let opened = open_store(&STAMPED, &disk.path(), Access::Read, None).expect("open");
        assert!(matches!(opened, Opened::Missing), "{opened:?}");
        assert!(!disk.path().exists());
    }

    #[test]
    fn stamped_write_of_no_file_creates_the_tables_and_both_stamps() {
        let disk = Disk::new();
        let conn = ready(open_store(&STAMPED, &disk.path(), Access::Write, None));
        assert_eq!(
            meta_get(&conn, "meta", "schema_version").expect("stamp"),
            Some("1".to_string())
        );
        assert_eq!(read_reader_stamp(&conn, &STAMP).expect("stamp"), Some(1));
    }

    #[test]
    fn stamped_read_of_a_current_store_is_ready_and_read_only() {
        let disk = Disk::current(&STAMPED);
        let conn = ready(open_store(&STAMPED, &disk.path(), Access::Read, None));
        assert!(!can_write(&conn));
    }

    #[test]
    fn stamped_write_of_a_current_store_is_ready() {
        let disk = Disk::current(&STAMPED);
        let conn = ready(open_store(&STAMPED, &disk.path(), Access::Write, None));
        assert!(can_write(&conn));
    }

    #[test]
    fn stamped_store_of_another_revision_must_be_rederived_under_both_accesses() {
        for access in STAMPED_ACCESSES {
            let disk = Disk::current(&STAMPED);
            meta_set(&disk.raw(), "meta", "schema_version", "0").expect("stamp");
            let err = refusal(open_store(&STAMPED, &disk.path(), access, None));
            assert!(
                matches!(&err, OpenError::Rederive { found, expected } if found == "0" && *expected == "1"),
                "{access:?}: {err:?}"
            );
        }
    }

    #[test]
    fn stamped_store_demanding_a_newer_reader_is_refused_under_both_accesses() {
        for access in STAMPED_ACCESSES {
            let disk = Disk::current(&STAMPED);
            let demanded = READER_VERSION + 1;
            meta_set(&disk.raw(), "meta", STAMP.key, &demanded.to_string()).expect("stamp");
            let err = refusal(open_store(&STAMPED, &disk.path(), access, None));
            assert!(
                matches!(err, OpenError::ReaderTooOld { required, .. } if required == demanded),
                "{access:?}: {err:?}"
            );
        }
    }

    #[test]
    fn stamped_store_with_drifted_tables_fails_verification_under_both_accesses() {
        for access in STAMPED_ACCESSES {
            let disk = Disk::current(&STAMPED);
            disk.raw()
                .execute_batch("ALTER TABLE item ADD COLUMN stray TEXT;")
                .expect("drift");
            let err = refusal(open_store(&STAMPED, &disk.path(), access, None));
            assert!(matches!(err, OpenError::Verify(_)), "{access:?}: {err:?}");
        }
    }

    #[test]
    #[should_panic(expected = "a stamped store has no bookkeeping access")]
    fn stamped_bookkeeping_is_unsupported() {
        let disk = Disk::current(&STAMPED);
        let _ = open_store(&STAMPED, &disk.path(), Access::Bookkeeping, None);
    }

    // ── Stamped: order of the checks ────────────────────────────────

    #[test]
    fn a_write_refused_on_its_stamp_leaves_the_schema_as_it_was() {
        // A store left by an older build: an old stamp, and tables the
        // newer spec has since widened. The stamp is compared before
        // any table is created, so the refusal names the stamp and the
        // file gains neither the new column's index nor the new table.
        let disk = Disk::current(&STAMPED);
        let before = disk.schema_rows();

        let err = refusal(open_store(&STAMPED_NEXT, &disk.path(), Access::Write, None));
        assert!(
            matches!(&err, OpenError::Rederive { found, expected } if found == "1" && *expected == "2"),
            "{err:?}"
        );
        assert_eq!(disk.schema_rows(), before);
    }

    #[test]
    fn a_write_refused_on_the_reader_stamp_leaves_the_schema_as_it_was() {
        let disk = Disk::current(&STAMPED);
        let demanded = READER_VERSION + 1;
        let raw = disk.raw();
        meta_set(&raw, "meta", STAMP.key, &demanded.to_string()).expect("stamp");
        // The stamp the newer spec expects, so only the reader axis objects.
        meta_set(&raw, "meta", "schema_version", "2").expect("stamp");
        drop(raw);
        let before = disk.schema_rows();

        let err = refusal(open_store(&STAMPED_NEXT, &disk.path(), Access::Write, None));
        assert!(matches!(err, OpenError::ReaderTooOld { .. }), "{err:?}");
        assert_eq!(disk.schema_rows(), before);
    }

    #[test]
    fn a_read_names_the_reader_stamp_before_the_table_shapes() {
        let disk = Disk::current(&STAMPED);
        let demanded = READER_VERSION + 1;
        let raw = disk.raw();
        meta_set(&raw, "meta", STAMP.key, &demanded.to_string()).expect("stamp");
        raw.execute_batch("ALTER TABLE item ADD COLUMN stray TEXT;")
            .expect("drift");
        drop(raw);

        let err = refusal(open_store(&STAMPED, &disk.path(), Access::Read, None));
        assert!(matches!(err, OpenError::ReaderTooOld { .. }), "{err:?}");
    }

    #[test]
    fn a_read_of_a_file_without_the_meta_table_fails_verification() {
        let disk = Disk::new();
        drop(Connection::open(disk.path()).expect("an uninitialised database"));
        let err = refusal(open_store(&STAMPED, &disk.path(), Access::Read, None));
        assert!(matches!(err, OpenError::Verify(_)), "{err:?}");
    }

    // ── Backups ─────────────────────────────────────────────────────

    #[test]
    fn a_backup_is_named_after_the_file_it_snapshots() {
        let dir = tempdir().expect("tempdir");
        let backups = dir.path().join("backups");
        for name in ["first.db", "second.db"] {
            let path = dir.path().join(name);
            let mut conn = Connection::open(&path).expect("seed");
            steps().to_version(&mut conn, 1).expect("stop at v1");
            drop(conn);
            ready(open_store(&MIGRATED, &path, Access::Write, Some(&backups)));
        }
        let mut names: Vec<String> = std::fs::read_dir(&backups)
            .expect("read backups")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names[0].starts_with("first-"), "{names:?}");
        assert!(names[1].starts_with("second-"), "{names:?}");
    }

    #[test]
    fn a_backup_directory_with_a_quote_in_its_name_takes_the_snapshot() {
        let disk = Disk::behind();
        let backups = disk.dir.path().join("it's here");
        ready(open_store(
            &MIGRATED,
            &disk.path(),
            Access::Write,
            Some(&backups),
        ));
        assert_eq!(std::fs::read_dir(&backups).expect("read").count(), 1);
    }

    // ── What counts as a missing file ───────────────────────────────

    #[test]
    fn a_directory_in_the_files_place_is_an_error_not_a_missing_store() {
        let disk = Disk::new();
        std::fs::create_dir(disk.path()).expect("a directory where the file belongs");
        for access in [Access::Read, Access::Bookkeeping] {
            assert!(
                open_store(&MIGRATED, &disk.path(), access, None).is_err(),
                "{access:?}"
            );
        }
    }

    #[test]
    fn open_existing_reports_a_missing_file_as_the_sqlite_failure() {
        let disk = Disk::new();
        let err = open_existing(&MIGRATED, &disk.path()).expect_err("a missing file");
        assert!(matches!(err, OpenError::Sqlite(_)), "{err:?}");
        assert!(!disk.path().exists());
    }

    #[test]
    fn open_existing_runs_the_checks_of_a_read() {
        let disk = Disk::behind();
        let err = open_existing(&MIGRATED, &disk.path()).expect_err("a behind store");
        assert!(matches!(err, OpenError::TooOld { found: 1, .. }), "{err:?}");

        let disk = Disk::current(&MIGRATED);
        let conn = open_existing(&MIGRATED, &disk.path()).expect("a current store");
        assert!(!can_write(&conn));
    }

    #[test]
    fn open_in_memory_builds_a_current_store_under_either_axis() {
        for spec in [&MIGRATED, &STAMPED] {
            let conn = open_in_memory(spec).expect("in-memory store");
            assert!(can_write(&conn));
            assert_eq!(read_reader_stamp(&conn, &STAMP).expect("stamp"), Some(1));
        }
    }

    #[test]
    fn the_rendered_fixture_tables_match_the_migrations() {
        // The migrated fixture is created by hand-written DDL and
        // verified against specs; this keeps the two from drifting.
        let conn = Connection::open_in_memory().expect("memory");
        conn.execute_batch(&format!("{}{}", render_ddl(&META), render_ddl(&ITEM)))
            .expect("render");
        verify_all(&conn, MIGRATED.tables).expect("rendered tables verify");
    }
}
