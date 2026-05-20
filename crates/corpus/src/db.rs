// SPDX-License-Identifier: Apache-2.0

//! The `corpus.db` connection handle, schema, and index-level scalars.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

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

/// Full schema for `corpus.db`. Idempotent: every statement uses
/// `IF NOT EXISTS`, so applying it to an up-to-date database is a no-op.
/// Compatibility across revisions is enforced separately by the
/// `schema_version` check, not by the DDL.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS nodes (
  node_id                INTEGER PRIMARY KEY,
  parent_id              INTEGER REFERENCES nodes(node_id) ON DELETE CASCADE,
  book_root_id           INTEGER NOT NULL,
  ordinal                INTEGER NOT NULL,
  depth                  INTEGER NOT NULL,
  node_type              TEXT NOT NULL,
  title                  TEXT,
  text_content           TEXT,
  char_count             INTEGER,
  sentence_count         INTEGER,
  toc_lo                 INTEGER,
  toc_hi                 INTEGER,
  page_index_start       INTEGER,
  page_index_end         INTEGER,
  stable_anchor          TEXT,
  text_sha256            TEXT,
  norm_text_sha256       TEXT,
  subtree_content_sha256 TEXT,
  expression_id          INTEGER
);

CREATE INDEX IF NOT EXISTS idx_node_root
  ON nodes(book_root_id, parent_id, ordinal);
CREATE INDEX IF NOT EXISTS idx_node_parent
  ON nodes(parent_id, ordinal);
CREATE INDEX IF NOT EXISTS idx_node_type
  ON nodes(node_type);
CREATE INDEX IF NOT EXISTS idx_node_norm_sha
  ON nodes(norm_text_sha256) WHERE norm_text_sha256 IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_node_subtree_sig
  ON nodes(subtree_content_sha256) WHERE subtree_content_sha256 IS NOT NULL;

CREATE TABLE IF NOT EXISTS node_id_partitions (
  partition_idx INTEGER PRIMARY KEY,
  book_root_id  INTEGER NOT NULL UNIQUE,
  intake_id     INTEGER NOT NULL UNIQUE,
  next_local_id INTEGER NOT NULL DEFAULT 2,
  allocated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS index_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

/// A handle to one `corpus.db` database.
///
/// Owns a single SQLite connection. Construct with [`Corpus::open`] for
/// a file-backed database or [`Corpus::open_in_memory`] for an
/// ephemeral one (useful in tests and for throwaway processing).
pub struct Corpus {
    pub(crate) conn: Connection,
}

impl Corpus {
    /// Open the `corpus.db` at `path`, creating and initializing it if
    /// it does not exist.
    ///
    /// Fails with [`CorpusError::SchemaMismatch`] if the file exists but
    /// was built by an incompatible schema revision.
    pub fn open(path: &Path) -> Result<Corpus> {
        Corpus::from_connection(Connection::open(path)?)
    }

    /// Open an ephemeral, private `corpus.db` held entirely in memory.
    /// The database vanishes when the handle is dropped.
    pub fn open_in_memory() -> Result<Corpus> {
        Corpus::from_connection(Connection::open_in_memory()?)
    }

    /// Apply per-connection pragmas, ensure the schema is present, and
    /// reconcile the schema version.
    fn from_connection(conn: Connection) -> Result<Corpus> {
        // Foreign keys are off by default and the setting is not
        // persisted, so it must be re-enabled on every connection.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        let corpus = Corpus { conn };
        corpus.reconcile_schema_version()?;
        Ok(corpus)
    }

    /// Stamp the schema version on a fresh database, or verify it on an
    /// existing one.
    fn reconcile_schema_version(&self) -> Result<()> {
        let Some(found) = self.meta_get(SCHEMA_VERSION_KEY)? else {
            self.meta_set(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_string())?;
            return Ok(());
        };
        if found.parse::<u32>().is_ok_and(|v| v == SCHEMA_VERSION) {
            Ok(())
        } else {
            Err(CorpusError::SchemaMismatch {
                found,
                expected: SCHEMA_VERSION,
            })
        }
    }

    /// Read an `index_meta` scalar, or `None` if the key is unset.
    ///
    /// `index_meta` records the parameters an index was built with —
    /// embedding model, vector dimension, chunk and normalization
    /// versions — so a daemon can refuse to serve an index that no
    /// longer matches its compiled-in constants.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = ?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(value)
    }

    /// Write an `index_meta` scalar, replacing any previous value.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO index_meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
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
}
