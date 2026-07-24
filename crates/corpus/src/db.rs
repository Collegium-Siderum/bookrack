// SPDX-License-Identifier: Apache-2.0

//! The `corpus.db` connection handle, schema, and index-level scalars.

use std::path::Path;

use bookrack_dbkit::{
    OpenDecision, READER_VERSION, TableSpec, TimedConnection, apply_schema, reader_version_decision,
};
use rusqlite::Connection;

use crate::{CorpusError, Result};

/// Revision of the `corpus.db` schema this binary creates and accepts.
///
/// Stored in `index_meta` under `schema_version` when a database is
/// first created, and checked against on every subsequent open. There
/// are no migrations: a mismatch is resolved by rebuilding the corpus,
/// which is cheap because the store is rebuildable by design.
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

/// A handle to one `corpus.db` database.
///
/// Owns a single SQLite connection. Construct with [`Corpus::open`] for
/// a file-backed database or [`Corpus::open_in_memory`] for an
/// ephemeral one (useful in tests and for throwaway processing).
pub struct Corpus {
    pub(crate) conn: TimedConnection,
}

impl Corpus {
    /// Open the `corpus.db` at `path`, creating and initializing it if
    /// it does not exist.
    ///
    /// Fails with [`CorpusError::SchemaMismatch`] if the file exists but
    /// was built by an incompatible schema revision.
    pub fn open(path: &Path) -> Result<Corpus> {
        Corpus::from_connection(bookrack_dbkit::open_production(path)?)
    }

    /// Open an ephemeral, private `corpus.db` held entirely in memory.
    /// The database vanishes when the handle is dropped.
    pub fn open_in_memory() -> Result<Corpus> {
        Corpus::from_connection(Connection::open_in_memory()?)
    }

    /// Open the `corpus.db` at `path` without creating it and without
    /// writing through the connection.
    ///
    /// Mirrors [`Catalog::open_read_only`][bookrack_catalog::Catalog::open_read_only]:
    /// the `query_only` PRAGMA blocks writes and the stamp *writers* are
    /// skipped, but every read-side check the read-write path runs still
    /// runs here — the table-spec conformance check, the `schema_version`
    /// stamp, and the `min_reader_version` stamp — so a drifted or
    /// incompatible file is refused at open. A missing file is reported as
    /// a SQLite open failure rather than silently materialized.
    pub fn open_read_only(path: &Path) -> Result<Corpus> {
        let conn = bookrack_dbkit::open_production_strict_read_only(path)?;
        bookrack_dbkit::verify_all(&conn, SPECS).map_err(CorpusError::Verify)?;
        let corpus = Corpus {
            conn: TimedConnection::new(conn, "corpus"),
        };
        // Verify the schema-version stamp without writing: a missing stamp
        // (fresh file) is accepted as-is, a differing revision is refused.
        let found = corpus.meta_get(SCHEMA_VERSION_KEY)?;
        if let OpenDecision::Rederive { .. } = decide(found.as_deref()) {
            return Err(CorpusError::SchemaMismatch {
                found: found.unwrap_or_default(),
                expected: SCHEMA_VERSION,
            });
        }
        let stored = corpus.read_min_reader_version()?;
        if let OpenDecision::Refuse { .. } = reader_version_decision(stored) {
            return Err(CorpusError::ReaderTooOld {
                required: stored.expect("Refuse implies a stamp was present"),
                current: READER_VERSION,
            });
        }
        Ok(corpus)
    }

    /// Apply per-connection pragmas, ensure the schema is present, and
    /// reconcile the schema version.
    fn from_connection(conn: Connection) -> Result<Corpus> {
        // Foreign keys are off by default and the setting is not
        // persisted, so it must be re-enabled on every connection.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        apply_schema(&conn, SPECS)?;
        // In debug builds, fail loudly if an existing file's schema has
        // drifted from the specs. A freshly built database always
        // conforms, so this only bites on a stale file — which a release
        // build instead catches through the version stamp.
        #[cfg(debug_assertions)]
        bookrack_dbkit::verify_all(&conn, SPECS).expect("corpus.db schema conformance");
        let corpus = Corpus {
            conn: TimedConnection::new(conn, "corpus"),
        };
        corpus.reconcile_schema_version()?;
        corpus.reconcile_reader_version()?;
        Ok(corpus)
    }

    /// Refuse a `min_reader_version` stamp this build cannot meet, and
    /// seed the stamp when missing.
    fn reconcile_reader_version(&self) -> Result<()> {
        let stored = self.read_min_reader_version()?;
        match reader_version_decision(stored) {
            OpenDecision::Refuse { .. } => Err(CorpusError::ReaderTooOld {
                required: stored.expect("Refuse implies a stamp was present"),
                current: READER_VERSION,
            }),
            OpenDecision::Match => {
                if stored.is_none() {
                    self.meta_set(MIN_READER_VERSION_KEY, &MIN_READER_VERSION.to_string())?;
                }
                Ok(())
            }
            OpenDecision::Migrate { .. } | OpenDecision::Rederive { .. } => {
                unreachable!("reader_version_decision emits only Match or Refuse")
            }
        }
    }

    /// Read the recorded `min_reader_version` stamp from `index_meta`,
    /// returning `None` if no row has been written yet.
    fn read_min_reader_version(&self) -> Result<Option<u32>> {
        Ok(self
            .meta_get(MIN_READER_VERSION_KEY)?
            .and_then(|s| s.parse::<u32>().ok()))
    }

    /// Stamp the schema version on a fresh database, or verify it on an
    /// existing one.
    fn reconcile_schema_version(&self) -> Result<()> {
        let found = self.meta_get(SCHEMA_VERSION_KEY)?;
        match decide(found.as_deref()) {
            OpenDecision::Match => {
                if found.is_none() {
                    self.meta_set(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_string())?;
                }
                Ok(())
            }
            OpenDecision::Rederive { .. } => Err(CorpusError::SchemaMismatch {
                found: found.unwrap_or_default(),
                expected: SCHEMA_VERSION,
            }),
            // `corpus.db` is rebuildable from sources and carries no
            // forward-only migration sequence, so a mismatch is resolved
            // by `bookrack corpus rebuild`, not by migrating forward;
            // and there is no separate axis for the open path to refuse
            // on at this point.
            OpenDecision::Migrate { .. } | OpenDecision::Refuse { .. } => {
                unreachable!("corpus.db open path emits only Match or Rederive")
            }
        }
    }
}

/// Classify a corpus database carrying the optional stamped
/// `schema_version` value `found` into one of the four self-check
/// verdicts.
///
/// `corpus.db` is rebuildable from sources, so its decision tree is
/// the rebuildable-store shape: a missing stamp means a fresh database
/// (open as [`OpenDecision::Match`], the caller will stamp it); an
/// equal stamp also matches; anything else is a [`OpenDecision::Rederive`]
/// — the resolution is `bookrack corpus rebuild`, not a migration.
fn decide(found: Option<&str>) -> OpenDecision {
    match found {
        None => OpenDecision::Match,
        Some(value) if value.parse::<u32>().is_ok_and(|v| v == SCHEMA_VERSION) => {
            OpenDecision::Match
        }
        Some(_) => OpenDecision::Rederive {
            reason: "corpus schema version disagrees with this binary",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let corpus = Corpus::open(&path).expect("first open");
            corpus
                .meta_set(MIN_READER_VERSION_KEY, &too_new.to_string())
                .expect("overwrite stamp with a too-new value");
        }

        let Err(err) = Corpus::open(&path) else {
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

        Corpus::open(&path).expect("first open");
        // Scope the reopened handle so its connection is closed before
        // the cleanup: Windows refuses to delete a file still held open.
        let version = {
            let reopened = Corpus::open(&path).expect("second open");
            reopened.meta_get(SCHEMA_VERSION_KEY).expect("read")
        };
        assert_eq!(version, Some(SCHEMA_VERSION.to_string()));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_foreign_schema_version_is_rejected() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus
            .meta_set(SCHEMA_VERSION_KEY, "999")
            .expect("overwrite version");
        // A second open of the same in-memory connection is not
        // possible, so re-run the check directly.
        let err = corpus.reconcile_schema_version().expect_err("must reject");
        assert!(matches!(err, CorpusError::SchemaMismatch { .. }));
    }

    #[test]
    fn open_read_only_checks_the_schema_version_stamp() {
        let dir =
            std::env::temp_dir().join(format!("bookrack-corpus-roschema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("corpus.db");

        Corpus::open(&path).expect("first open stamps the current schema version");
        // A matching stamp opens read-only cleanly.
        Corpus::open_read_only(&path).expect("read-only open of a matching corpus");

        // Overwrite the stamp with a revision this binary does not accept.
        // The table DDL is untouched, so the spec-conformance check the
        // read-only path already runs still passes; only the version stamp
        // catches the mismatch.
        let foreign = (SCHEMA_VERSION + 1).to_string();
        {
            let corpus = Corpus::open(&path).expect("reopen");
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

        Corpus::open(&path).expect("create");
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
            let corpus = Corpus::open(&path).expect("first open");
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
