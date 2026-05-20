// SPDX-License-Identifier: Apache-2.0

//! The `catalog.db` connection handle and schema.

use std::path::Path;

use bookrack_dbkit::{TableSpec, apply_schema};
use rusqlite::Connection;

use crate::{CatalogError, Result};

/// Revision of the `catalog.db` schema this binary creates and accepts.
///
/// Stamped into `catalog_meta` when a database is first created, and
/// checked on every subsequent open. `catalog.db` is the source of
/// truth and is not rebuildable, so a future schema change will need a
/// real migration; a mismatch is a hard error rather than a silent
/// recreation.
pub const SCHEMA_VERSION: u32 = 2;

/// `catalog_meta` key under which [`SCHEMA_VERSION`] is recorded.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Every `catalog.db` table that has a table module of its own. Their
/// schema is rendered from these specs and conformance-checked; there is
/// no separately maintained DDL for them. Compatibility across revisions
/// is enforced by the `schema_version` check, not by the DDL.
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
];

/// DDL for the `catalog.db` tables that do not yet have a table module.
/// Each will move into its own module — gaining a `TableSpec` and
/// conformance coverage — when its repository is built; until then its
/// schema lives here verbatim.
///
/// There are no foreign keys anywhere in `catalog.db`: every link — to a
/// `corpus.db` node, and even an expression to its work — is a bare
/// integer soft reference, keeping the two databases independently
/// movable and restorable.
const PENDING_TABLES_DDL: &str = r"

-- The six-stage pipeline log. Audit rows outlive the books they describe.
CREATE TABLE IF NOT EXISTS book_pipeline_audit (
  audit_id        INTEGER PRIMARY KEY AUTOINCREMENT,
  book_root_id    INTEGER,                  -- soft reference; NULL allowed
  source_sha256   TEXT,                     -- denormalized so the row survives book deletion
  stage           TEXT NOT NULL,
  sub_step        TEXT NOT NULL,
  outcome         TEXT NOT NULL,            -- ok / fail / partial / skipped
  adapter         TEXT,
  metric_summary  TEXT,                     -- JSON
  error_message   TEXT,
  duration_ms     INTEGER,
  ts              TEXT NOT NULL,
  pipeline_run_id TEXT NOT NULL,            -- ties one pipeline run together
  actor_kind      TEXT NOT NULL
    CHECK (actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')),
  actor_detail    TEXT,                     -- model name, import source, run id, ...
  session_id      TEXT
);
CREATE INDEX IF NOT EXISTS idx_pa_book    ON book_pipeline_audit(book_root_id, ts);
CREATE INDEX IF NOT EXISTS idx_pa_run     ON book_pipeline_audit(pipeline_run_id, ts);
CREATE INDEX IF NOT EXISTS idx_pa_stage   ON book_pipeline_audit(stage, ts);
CREATE INDEX IF NOT EXISTS idx_pa_outcome ON book_pipeline_audit(outcome, ts);

-- Audit trail of user metadata edits; supports history and undo.
CREATE TABLE IF NOT EXISTS metadata_audit (
  audit_id     INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id      INTEGER,                     -- soft reference; audit outlives the node
  table_name   TEXT NOT NULL,
  field        TEXT,                        -- NULL for a row-level insert/delete
  action       TEXT NOT NULL,
  old_value    TEXT,
  new_value    TEXT,
  changed_at   TEXT NOT NULL,
  actor_kind   TEXT NOT NULL
    CHECK (actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')),
  actor_detail TEXT,
  reason       TEXT,
  session_id   TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_node    ON metadata_audit(node_id, changed_at);
CREATE INDEX IF NOT EXISTS idx_audit_session ON metadata_audit(session_id);

-- FRBR identity (lightweight; empty in v1 by decision D3).
CREATE TABLE IF NOT EXISTS works (
  work_id    INTEGER PRIMARY KEY AUTOINCREMENT,
  title      TEXT,
  notes      TEXT,
  curated_at TEXT,
  curated_by TEXT
);
CREATE TABLE IF NOT EXISTS expressions (
  expression_id  INTEGER PRIMARY KEY AUTOINCREMENT,
  work_id        INTEGER,                   -- soft reference to works
  content_sha256 TEXT,                      -- content signature defining this expression's text
  kind           TEXT,                      -- translation / edition / printing
  label          TEXT,
  notes          TEXT,
  curated_at     TEXT,
  curated_by     TEXT
);
CREATE INDEX IF NOT EXISTS idx_expr_content
  ON expressions(content_sha256) WHERE content_sha256 IS NOT NULL;

-- Authoritative log of manual TOC edits. The corpus.db node tree is a
-- materialized projection of the extracted skeleton plus this overlay,
-- so a corpus rebuild replays these verbs and never loses an edit.
CREATE TABLE IF NOT EXISTS toc_edits (
  edit_id       INTEGER PRIMARY KEY AUTOINCREMENT,
  book_root_id  INTEGER NOT NULL,           -- soft reference to corpus.nodes
  seq           INTEGER NOT NULL,           -- per-book edit order; replay sorts by this
  verb          TEXT NOT NULL,              -- split / merge / set_range / rename / set_type / new / rm
  args          TEXT NOT NULL,              -- JSON verb arguments
  target_anchor TEXT,                       -- content fingerprint, to re-locate the target on replay
  new_node_id   INTEGER,                    -- id of an org node created by new/split; reused on replay
  actor_kind    TEXT NOT NULL
    CHECK (actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')),
  actor_detail  TEXT,
  edited_at     TEXT NOT NULL,
  session_id    TEXT,
  UNIQUE (book_root_id, seq)
);

-- Observability: the MCP / CLI tool-call log.
CREATE TABLE IF NOT EXISTS mcp_tool_calls (
  call_id     INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          TEXT NOT NULL,
  source      TEXT NOT NULL,                -- mcp / cli
  tool        TEXT NOT NULL,
  status      TEXT NOT NULL,                -- ok / error
  duration_ms REAL,
  session_id  TEXT,
  error_type  TEXT,
  error_msg   TEXT,
  args        TEXT,                         -- JSON
  timings_ms  TEXT,                         -- JSON
  extras      TEXT                          -- JSON
);
CREATE INDEX IF NOT EXISTS idx_mcp_tool_ts ON mcp_tool_calls(tool, ts);

-- Observability: retrieval-quality issue reports.
CREATE TABLE IF NOT EXISTS retrieval_issues (
  issue_id       INTEGER PRIMARY KEY AUTOINCREMENT,
  created_at     TEXT NOT NULL,
  updated_at     TEXT NOT NULL,
  status         TEXT NOT NULL DEFAULT 'open',   -- open / triaged / resolved / wontfix
  kind           TEXT NOT NULL,                  -- recall_miss / zero_hits / wrong_volume / ...
  severity       TEXT NOT NULL DEFAULT 'medium',
  query          TEXT,
  query_hash     TEXT,
  mode           TEXT,
  filters_json   TEXT,
  expected       TEXT,
  observed       TEXT,
  suspected_book TEXT,
  agent_notes    TEXT,
  seen_count     INTEGER NOT NULL DEFAULT 1,
  resolution     TEXT,
  resolved_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_issues_status ON retrieval_issues(status, created_at);
CREATE INDEX IF NOT EXISTS idx_issues_dedup
  ON retrieval_issues(query_hash) WHERE status = 'open';
";

/// A handle to one `catalog.db` database.
///
/// Owns a single SQLite connection. Construct with [`Catalog::open`]
/// for a file-backed database or [`Catalog::open_in_memory`] for an
/// ephemeral one (useful in tests).
pub struct Catalog {
    pub(crate) conn: Connection,
}

impl Catalog {
    /// Open the `catalog.db` at `path`, creating and initializing it if
    /// it does not exist.
    ///
    /// Fails with [`CatalogError::SchemaMismatch`] if the file exists
    /// but was built by an incompatible schema revision.
    pub fn open(path: &Path) -> Result<Catalog> {
        Catalog::from_connection(Connection::open(path)?)
    }

    /// Open an ephemeral, private `catalog.db` held entirely in memory.
    /// The database vanishes when the handle is dropped.
    pub fn open_in_memory() -> Result<Catalog> {
        Catalog::from_connection(Connection::open_in_memory()?)
    }

    /// Apply per-connection pragmas, ensure the schema is present, and
    /// reconcile the schema version.
    fn from_connection(conn: Connection) -> Result<Catalog> {
        // The schema has no foreign keys today, but enabling the pragma
        // keeps the connection ready to enforce any added later. It is
        // off by default and not persisted, so it is set on every open.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        apply_schema(&conn, SPECS)?;
        conn.execute_batch(PENDING_TABLES_DDL)?;
        // In debug builds, fail loudly if an existing file's schema has
        // drifted from the specs. The pending tables carry no spec yet
        // and so are not covered.
        #[cfg(debug_assertions)]
        bookrack_dbkit::verify_all(&conn, SPECS).expect("catalog.db schema conformance");
        let catalog = Catalog { conn };
        catalog.reconcile_schema_version()?;
        Ok(catalog)
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
            Err(CatalogError::SchemaMismatch {
                found,
                expected: SCHEMA_VERSION,
            })
        }
    }
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
    fn a_foreign_schema_version_is_rejected() {
        let catalog = Catalog::open_in_memory().expect("open");
        catalog
            .meta_set(SCHEMA_VERSION_KEY, "999")
            .expect("overwrite version");
        let err = catalog.reconcile_schema_version().expect_err("must reject");
        assert!(matches!(err, CatalogError::SchemaMismatch { .. }));
    }

    #[test]
    fn the_built_schema_conforms_to_every_spec() {
        // Proves the DDL rendered from the specs builds a database whose
        // live schema matches those same specs.
        let catalog = Catalog::open_in_memory().expect("open");
        bookrack_dbkit::verify_all(&catalog.conn, SPECS)
            .expect("the rendered schema must conform to every spec");
    }
}
