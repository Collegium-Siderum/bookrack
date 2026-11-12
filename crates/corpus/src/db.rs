// SPDX-License-Identifier: Apache-2.0

//! The `corpus.db` connection handle, schema, and index-level scalars.

use std::path::Path;

use bookrack_dbkit::{
    Access, OpenError, Opened, ReaderStamp, SchemaAxis, StoreSpec, TableSpec, TimedConnection,
};
use rusqlite::Connection;

use crate::{CorpusError, Result};

/// Revision of the `corpus.db` schema this binary creates and accepts.
///
/// Stored in `index_meta` under `schema_version` when a database is
/// first created, and checked against on every subsequent open. There
/// are no migrations: a mismatch is resolved by rebuilding the corpus,
/// which is cheap because the store is rebuildable by design.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const SCHEMA_VERSION: u32 = 1;

/// `index_meta` key under which [`SCHEMA_VERSION`] is recorded.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// The `min_reader_version` value this binary stamps when writing
/// `corpus.db`.
///
/// Bump when a writer-side change to `corpus.db` would make older
/// readers misinterpret the data — e.g. repurposing a column or changing
/// the meaning of a stamp value. Additive changes to the node tree or
/// new `index_meta` keys do not require a bump.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const MIN_READER_VERSION: u32 = 1;

/// `index_meta` key under which [`MIN_READER_VERSION`] is recorded.
const MIN_READER_VERSION_KEY: &str = "min_reader_version";

/// Every table `corpus.db` owns, in creation order. The schema is built
/// by rendering these specs, and the same list drives the conformance
/// check — there is no separately maintained DDL string that could drift
/// from the code. Compatibility across revisions is enforced by the
/// `schema_version` check, not by the DDL.
const SPECS: &[&TableSpec] = &[
    &crate::node::SPEC,
    &crate::partition::SPEC,
    &crate::index_meta::SPEC,
];

/// `corpus.db` as the open protocol sees it.
const STORE: StoreSpec = StoreSpec {
    label: "corpus",
    tables: SPECS,
    schema: SchemaAxis::Stamped {
        key: SCHEMA_VERSION_KEY,
        value: SCHEMA_VERSION_STAMP,
    },
    reader_stamp: ReaderStamp {
        table: "index_meta",
        key: MIN_READER_VERSION_KEY,
        write_version: MIN_READER_VERSION,
    },
};

/// [`SCHEMA_VERSION`] as it is written to `index_meta`.
const SCHEMA_VERSION_STAMP: &str = "1";

impl From<OpenError> for CorpusError {
    fn from(e: OpenError) -> CorpusError {
        match e {
            OpenError::Rederive { found, .. } => CorpusError::SchemaMismatch {
                found,
                expected: SCHEMA_VERSION,
            },
            OpenError::ReaderTooOld { required, current } => {
                CorpusError::ReaderTooOld { required, current }
            }
            OpenError::Verify(e) => CorpusError::Verify(e),
            OpenError::Sqlite(e) => CorpusError::Sqlite(e),
            OpenError::TooOld { .. } | OpenError::TooNew { .. } | OpenError::Migrate(_) => {
                unreachable!("corpus.db is stamped, never migrated")
            }
            OpenError::Io(_) => unreachable!("corpus.db takes no backup"),
        }
    }
}

/// A handle to one `corpus.db` database.
///
/// Owns a single SQLite connection. [`Corpus::open`] and
/// [`Corpus::open_read_only`] look, [`Corpus::open_for_write`] creates
/// the file if it is missing, and [`Corpus::open_in_memory`] gives an
/// ephemeral one (useful in tests and for throwaway processing).
pub struct Corpus {
    pub(crate) conn: TimedConnection,
}

impl Corpus {
    /// Open the `corpus.db` at `path` read-only, if there is one.
    ///
    /// `Ok(None)` when the filesystem reports no file at `path`.
    /// Looking never creates the file and never writes a stamp. A file
    /// stamped with another schema revision is refused with
    /// [`CorpusError::SchemaMismatch`].
    pub fn open(path: &Path) -> Result<Option<Corpus>> {
        match bookrack_dbkit::open_store(&STORE, path, Access::Read, None)? {
            Opened::Ready(conn) => Ok(Some(Corpus::wrap(conn))),
            Opened::Missing | Opened::Declined => Ok(None),
        }
    }

    /// Open the `corpus.db` at `path` for writing, creating and
    /// initializing it if it does not exist.
    ///
    /// Fails with [`CorpusError::SchemaMismatch`] if the file exists but
    /// was built by an incompatible schema revision. The stamps are
    /// compared before any table is created, so a refused file gains
    /// nothing from the attempt.
    pub fn open_for_write(path: &Path) -> Result<Corpus> {
        match bookrack_dbkit::open_store(&STORE, path, Access::Write, None)? {
            Opened::Ready(conn) => Ok(Corpus::wrap(conn)),
            Opened::Missing | Opened::Declined => unreachable!("a write door creates"),
        }
    }

    /// Open an ephemeral, private `corpus.db` held entirely in memory.
    /// The database vanishes when the handle is dropped.
    pub fn open_in_memory() -> Result<Corpus> {
        Ok(Corpus::wrap(bookrack_dbkit::open_in_memory(&STORE)?))
    }

    /// Open the `corpus.db` at `path` read-only, requiring the file.
    ///
    /// Mirrors [`Catalog::open_read_only`][bookrack_catalog::Catalog::open_read_only]:
    /// the `query_only` PRAGMA blocks writes and no stamp is written,
    /// but every check the write door runs still runs here — the
    /// `schema_version` stamp, the `min_reader_version` stamp, and the
    /// table-spec conformance check — so a drifted or incompatible file
    /// is refused at open. A missing file is reported as a SQLite open
    /// failure rather than silently materialized.
    pub fn open_read_only(path: &Path) -> Result<Corpus> {
        Ok(Corpus::wrap(bookrack_dbkit::open_existing(&STORE, path)?))
    }

    fn wrap(conn: Connection) -> Corpus {
        Corpus {
            conn: TimedConnection::new(conn, STORE.label),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_dbkit::READER_VERSION;

    /// A fresh directory under the system temp dir, unique to this
    /// process and `tag`.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bookrack-corpus-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_stamp_the_protocol_compares_is_the_schema_version() {
        assert_eq!(SCHEMA_VERSION_STAMP, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn fresh_database_stamps_the_schema_version() {
        let corpus = Corpus::open_in_memory().expect("open");
        assert_eq!(
            corpus.meta_get(SCHEMA_VERSION_KEY).expect("read"),
            Some(SCHEMA_VERSION.to_string())
        );
    }

    #[test]
    fn fresh_database_stamps_the_min_reader_version() {
        let corpus = Corpus::open_in_memory().expect("open");
        assert_eq!(
            corpus.meta_get(MIN_READER_VERSION_KEY).expect("read"),
            Some(MIN_READER_VERSION.to_string())
        );
    }

    #[test]
    fn open_refuses_a_stamp_above_this_binarys_reader_version() {
        let dir =
            std::env::temp_dir().join(format!("bookrack-corpus-reader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("corpus.db");

        let too_new = READER_VERSION + 1;
        {
            let corpus = Corpus::open_for_write(&path).expect("first open");
            corpus
                .meta_set(MIN_READER_VERSION_KEY, &too_new.to_string())
                .expect("overwrite stamp with a too-new value");
        }

        let Err(err) = Corpus::open_for_write(&path) else {
            panic!("reopen must refuse")
        };
        assert!(
            matches!(err, CorpusError::ReaderTooOld { required, current }
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
        let dir = std::env::temp_dir().join(format!("bookrack-corpus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("corpus.db");

        Corpus::open_for_write(&path).expect("first open");
        // Scope the reopened handle so its connection is closed before
        // the cleanup: Windows refuses to delete a file still held open.
        let version = {
            let reopened = Corpus::open_for_write(&path).expect("second open");
            reopened.meta_get(SCHEMA_VERSION_KEY).expect("read")
        };
        assert_eq!(version, Some(SCHEMA_VERSION.to_string()));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_foreign_schema_version_is_rejected() {
        let dir = scratch_dir("foreign");
        let path = dir.join("corpus.db");
        Corpus::open_for_write(&path)
            .expect("create")
            .meta_set(SCHEMA_VERSION_KEY, "999")
            .expect("overwrite version");

        let Err(err) = Corpus::open_for_write(&path) else {
            panic!("a foreign revision must be refused")
        };
        assert!(
            matches!(&err, CorpusError::SchemaMismatch { found, expected }
                if found == "999" && *expected == SCHEMA_VERSION),
            "{err:?}"
        );
    }

    #[test]
    fn open_reports_a_missing_file_as_absent_without_creating_it() {
        let dir = scratch_dir("absent");
        let path = dir.join("corpus.db");

        let opened = Corpus::open(&path).expect("a missing file is not an error");
        assert!(opened.is_none());
        assert!(!path.exists(), "looking must not create corpus.db");
    }

    #[test]
    fn open_reads_an_existing_corpus_and_rejects_writes() {
        let dir = scratch_dir("reads");
        let path = dir.join("corpus.db");
        drop(Corpus::open_for_write(&path).expect("create"));

        let corpus = Corpus::open(&path).expect("open").expect("present");
        assert_eq!(
            corpus.meta_get(SCHEMA_VERSION_KEY).expect("read"),
            Some(SCHEMA_VERSION.to_string())
        );
        assert!(
            corpus.meta_set("probe", "x").is_err(),
            "a handle from the read door must not write"
        );
    }

    #[test]
    fn open_read_only_checks_the_schema_version_stamp() {
        let dir =
            std::env::temp_dir().join(format!("bookrack-corpus-roschema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("corpus.db");

        Corpus::open_for_write(&path).expect("first open stamps the current schema version");
        // A matching stamp opens read-only cleanly.
        Corpus::open_read_only(&path).expect("read-only open of a matching corpus");

        // Overwrite the stamp with a revision this binary does not accept.
        // The table DDL is untouched, so the spec-conformance check the
        // read-only path already runs still passes; only the version stamp
        // catches the mismatch.
        let foreign = (SCHEMA_VERSION + 1).to_string();
        {
            let corpus = Corpus::open_for_write(&path).expect("reopen");
            corpus
                .meta_set(SCHEMA_VERSION_KEY, &foreign)
                .expect("overwrite schema_version");
        }

        match Corpus::open_read_only(&path) {
            Err(CorpusError::SchemaMismatch { found, expected }) => {
                assert_eq!(found, foreign);
                assert_eq!(expected, SCHEMA_VERSION);
            }
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("read-only open must refuse a mismatched schema version"),
        }

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn open_read_only_refuses_a_missing_file_without_creating_it() {
        let dir =
            std::env::temp_dir().join(format!("bookrack-corpus-romissing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("corpus.db");

        let Err(err) = Corpus::open_read_only(&path) else {
            panic!("a missing file must not open")
        };
        assert!(
            matches!(err, CorpusError::Sqlite(_)),
            "unexpected error: {err:?}"
        );
        assert!(
            !path.exists(),
            "a read-only open must not materialize the file"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn open_read_only_refuses_a_drifted_table_shape() {
        let dir =
            std::env::temp_dir().join(format!("bookrack-corpus-roverify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("corpus.db");

        Corpus::open_for_write(&path).expect("create");
        // Rename a column behind the specs' back. The version stamps are
        // untouched, so only the spec-conformance check can catch this.
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch("ALTER TABLE nodes RENAME COLUMN expression_id TO expr_id")
                .expect("rename column");
        }

        let Err(err) = Corpus::open_read_only(&path) else {
            panic!("a drifted schema must be refused")
        };
        assert!(
            matches!(err, CorpusError::Verify(_)),
            "unexpected error: {err:?}"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn open_read_only_refuses_a_stamp_above_this_binarys_reader_version() {
        let dir =
            std::env::temp_dir().join(format!("bookrack-corpus-roreader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("corpus.db");

        let too_new = READER_VERSION + 1;
        {
            let corpus = Corpus::open_for_write(&path).expect("first open");
            corpus
                .meta_set(MIN_READER_VERSION_KEY, &too_new.to_string())
                .expect("overwrite stamp with a too-new value");
        }

        let Err(err) = Corpus::open_read_only(&path) else {
            panic!("read-only reopen must refuse")
        };
        assert!(
            matches!(err, CorpusError::ReaderTooOld { required, current }
                if required == too_new && current == READER_VERSION),
            "unexpected error: {err:?}"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn index_meta_round_trips_and_overwrites() {
        let corpus = Corpus::open_in_memory().expect("open");
        assert_eq!(corpus.meta_get("embed_model").expect("get"), None);
        corpus.meta_set("embed_model", "qwen3").expect("set");
        assert_eq!(
            corpus.meta_get("embed_model").expect("get"),
            Some("qwen3".to_string())
        );
        corpus
            .meta_set("embed_model", "qwen3-v2")
            .expect("overwrite");
        assert_eq!(
            corpus.meta_get("embed_model").expect("get"),
            Some("qwen3-v2".to_string())
        );
    }

    #[test]
    fn the_built_schema_conforms_to_every_spec() {
        // Proves the DDL rendered from the specs builds a database whose
        // live schema — columns, keys, indexes, foreign keys — matches
        // those same specs.
        let corpus = Corpus::open_in_memory().expect("open");
        bookrack_dbkit::verify_all(&corpus.conn, SPECS)
            .expect("the rendered schema must conform to every spec");
    }
}
