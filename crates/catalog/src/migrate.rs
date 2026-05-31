// SPDX-License-Identifier: Apache-2.0

//! The `catalog.db` schema migration sequence.
//!
//! `catalog.db` is source-of-truth and cannot be rebuilt, so its schema
//! evolves through forward-only migrations rather than a drop-and-recreate.
//! The applied revision lives in SQLite's `user_version`, advanced by
//! `rusqlite_migration`.
//!
//! `M[0]` is the frozen baseline: the schema as of the migration
//! framework's introduction, captured once from the rendered
//! [`TableSpec`](bookrack_dbkit::TableSpec)s and never edited afterward.
//! Each later change is layered on as its own migration (`M[1]`, `M[2]`, …)
//! with literal SQL; the baseline is historical and is not re-rendered, so
//! a future transformative migration (e.g. a 12-step table rebuild)
//! composes correctly on top of it. The live specs, checked by `verify_all`
//! on open, stay the source of truth for the *current* schema shape; the
//! baseline below is the *historical* one.

use rusqlite_migration::{M, Migrations};

/// The `user_version` a fully-migrated `catalog.db` carries: the number of
/// migrations defined. The `catalog_meta.schema_version` mirror is kept
/// equal to it.
pub(crate) const TARGET_VERSION: i64 = 2;

/// `M[0]` — the frozen baseline schema (the former `schema_version` 3),
/// captured from the rendered specs. Immutable: never edit this text; add a
/// new migration instead.
const BASELINE_DDL: &str = r#"
-- Database-level scalars; currently just the schema version stamp.
CREATE TABLE IF NOT EXISTS catalog_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- A file manifestation: the identity anchor of one ingested source file.
CREATE TABLE IF NOT EXISTS intake (
  intake_id INTEGER PRIMARY KEY AUTOINCREMENT,  -- long-lived, never reused
  source_sha256 TEXT NOT NULL UNIQUE,  -- whole-file hash; the identity anchor
  stored_path TEXT,  -- opaque store location; set once the file is stored
  original_path TEXT,  -- forensic: where the file came from
  format TEXT,  -- pdf / epub / mobi / azw3 / text / ...
  byte_size INTEGER,
  adapter TEXT,  -- extraction adapter, stamped at EXTRACT
  extractor_version TEXT,  -- extractor version string, stamped at EXTRACT; a mismatch marks a stale partition
  intake_at TEXT NOT NULL,  -- ISO-8601 UTC
  status TEXT NOT NULL,  -- see IntakeStatus
  expression_id INTEGER,  -- FRBR soft reference; backfilled at METADATA
  notes TEXT
);
CREATE INDEX IF NOT EXISTS idx_intake_status ON intake(status);
CREATE INDEX IF NOT EXISTS idx_intake_format ON intake(format);

-- Book-level pipeline state, one row per ingested book.
CREATE TABLE IF NOT EXISTS book_state (
  book_root_id INTEGER PRIMARY KEY,  -- soft reference to corpus.nodes
  intake_id INTEGER NOT NULL UNIQUE,
  current_stage TEXT NOT NULL,
  embed_model TEXT,
  parsed_at TEXT,  -- STRUCTURE completed
  embedded_at TEXT,  -- EMBED completed; non-NULL iff vectors exist
  ocr_marker_finished_at TEXT,
  last_error TEXT
);
CREATE INDEX IF NOT EXISTS idx_book_state_stage ON book_state(current_stage);
CREATE INDEX IF NOT EXISTS idx_book_state_embed ON book_state(embedded_at) WHERE embedded_at IS NULL;

-- Extracted bibliographic attributes — the metadata base layer.
CREATE TABLE IF NOT EXISTS node_publication_attrs (
  node_id INTEGER PRIMARY KEY,  -- soft reference to corpus.nodes
  title TEXT,
  subtitle TEXT,
  publisher TEXT,
  year TEXT,
  publication_date TEXT,
  isbn TEXT,
  series TEXT,
  series_number TEXT,
  edition TEXT,
  language TEXT,
  original_title TEXT,  -- pre-FRBR stopgap: a translation's original-language title
  original_language TEXT,  -- pre-FRBR stopgap: the work's original language
  source_format TEXT,
  source TEXT,  -- ocr_marker / extracted / web
  confidence TEXT,  -- high / medium / low
  enriched_by TEXT
);

-- User EAV edits overriding the metadata base layer.
CREATE TABLE IF NOT EXISTS node_overrides (
  node_id INTEGER NOT NULL,
  field TEXT NOT NULL,
  value TEXT,  -- a value, or an explicit NULL meaning deliberate nullify
  confirmed INTEGER NOT NULL DEFAULT 0,
  curated_at TEXT NOT NULL,
  curated_by TEXT NOT NULL,
  notes TEXT,
  PRIMARY KEY (node_id, field)
);

-- Contributor roles (author / translator / editor / ...), many-to-many.
CREATE TABLE IF NOT EXISTS node_contributors (
  contributor_id INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id INTEGER NOT NULL,
  role TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  origin TEXT NOT NULL,  -- extracted / user
  name TEXT NOT NULL,
  nationality TEXT,
  inheritable INTEGER NOT NULL DEFAULT 1,
  UNIQUE (node_id, role, ordinal, origin)
);
CREATE INDEX IF NOT EXISTS idx_contrib_role_name ON node_contributors(role, name);

-- Explicit marker that the user has taken over a contributor role.
CREATE TABLE IF NOT EXISTS node_role_takeovers (
  node_id INTEGER NOT NULL,
  role TEXT NOT NULL,
  curated_at TEXT NOT NULL,
  curated_by TEXT NOT NULL,
  notes TEXT,
  PRIMARY KEY (node_id, role)
);

-- Category tags, many-to-many.
CREATE TABLE IF NOT EXISTS node_categories (
  node_id INTEGER NOT NULL,
  category TEXT NOT NULL,
  is_primary INTEGER NOT NULL DEFAULT 0,
  source TEXT NOT NULL,  -- user / llm_suggested / inferred
  confirmed INTEGER NOT NULL DEFAULT 0,
  curated_at TEXT NOT NULL,
  curated_by TEXT NOT NULL,
  PRIMARY KEY (node_id, category)
);
CREATE INDEX IF NOT EXISTS idx_cat_cat ON node_categories(category);

-- Per-node review status.
CREATE TABLE IF NOT EXISTS node_reviews (
  node_id INTEGER PRIMARY KEY,
  reviewed_at TEXT NOT NULL,
  reviewed_by TEXT NOT NULL,
  status TEXT NOT NULL,  -- clean / needs_work / rejected
  notes TEXT
);

-- Audit trail of user metadata edits; supports history and undo.
CREATE TABLE IF NOT EXISTS metadata_audit (
  audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
  node_id INTEGER,  -- soft reference; audit outlives the node
  table_name TEXT NOT NULL,
  field TEXT,  -- NULL for a row-level insert/delete
  action TEXT NOT NULL,
  old_value TEXT,
  new_value TEXT,
  changed_at TEXT NOT NULL,
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')),
  actor_detail TEXT,
  reason TEXT,
  session_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_node ON metadata_audit(node_id, changed_at);
CREATE INDEX IF NOT EXISTS idx_audit_session ON metadata_audit(session_id);

-- The six-stage pipeline log. Audit rows outlive the books they describe.
CREATE TABLE IF NOT EXISTS book_pipeline_audit (
  audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
  book_root_id INTEGER,  -- soft reference; NULL allowed
  source_sha256 TEXT,  -- denormalized so the row survives book deletion
  stage TEXT NOT NULL,
  sub_step TEXT NOT NULL,
  outcome TEXT NOT NULL,  -- ok / fail / partial / skipped
  adapter TEXT,
  metric_summary TEXT,  -- JSON
  error_message TEXT,
  duration_ms INTEGER,
  ts TEXT NOT NULL,
  pipeline_run_id TEXT NOT NULL,  -- ties one pipeline run together
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')),
  actor_detail TEXT,  -- model name, import source, run id, ...
  session_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_pa_book ON book_pipeline_audit(book_root_id, ts);
CREATE INDEX IF NOT EXISTS idx_pa_run ON book_pipeline_audit(pipeline_run_id, ts);
CREATE INDEX IF NOT EXISTS idx_pa_stage ON book_pipeline_audit(stage, ts);
CREATE INDEX IF NOT EXISTS idx_pa_outcome ON book_pipeline_audit(outcome, ts);

-- FRBR work identity (lightweight; empty in v1 by decision D3).
CREATE TABLE IF NOT EXISTS works (
  work_id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT,
  notes TEXT,
  curated_at TEXT,
  curated_by TEXT
);

-- One manifestation-class of a work: a translation, an edition.
CREATE TABLE IF NOT EXISTS expressions (
  expression_id INTEGER PRIMARY KEY AUTOINCREMENT,
  work_id INTEGER,  -- soft reference to works
  content_sha256 TEXT,  -- content signature defining this expression's text
  kind TEXT,  -- translation / edition / printing
  label TEXT,
  notes TEXT,
  curated_at TEXT,
  curated_by TEXT
);
CREATE INDEX IF NOT EXISTS idx_expr_content ON expressions(content_sha256) WHERE content_sha256 IS NOT NULL;

-- Observability: the MCP / CLI tool-call log.
CREATE TABLE IF NOT EXISTS mcp_tool_calls (
  call_id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  source TEXT NOT NULL,  -- mcp / cli
  tool TEXT NOT NULL,
  status TEXT NOT NULL,  -- ok / error
  duration_ms REAL,
  session_id TEXT,
  error_type TEXT,
  error_msg TEXT,
  args TEXT,  -- JSON
  timings_ms TEXT,  -- JSON
  extras TEXT  -- JSON
);
CREATE INDEX IF NOT EXISTS idx_mcp_tool_ts ON mcp_tool_calls(tool, ts);

-- Observability: retrieval-quality issue reports.
CREATE TABLE IF NOT EXISTS retrieval_issues (
  issue_id INTEGER PRIMARY KEY AUTOINCREMENT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',  -- open / triaged / resolved / wontfix
  kind TEXT NOT NULL,  -- recall_miss / zero_hits / wrong_volume / ...
  severity TEXT NOT NULL DEFAULT 'medium',
  query TEXT,
  query_hash TEXT,
  mode TEXT,
  filters_json TEXT,
  expected TEXT,
  observed TEXT,
  suspected_book TEXT,
  agent_notes TEXT,
  seen_count INTEGER NOT NULL DEFAULT 1,
  resolution TEXT,
  resolved_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_issues_status ON retrieval_issues(status, created_at);
CREATE INDEX IF NOT EXISTS idx_issues_dedup ON retrieval_issues(query_hash) WHERE status = 'open';



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

"#;

/// `M[1]` — covering index for the contributor read path, which looks up
/// `node_contributors` by node, then role, then ordinal. The first real
/// migration layered on the baseline.
const CONTRIBUTOR_INDEX_DDL: &str =
    "CREATE INDEX idx_contrib_node ON node_contributors(node_id, role, ordinal);";

/// The migration sequence applied to `catalog.db` on open. Forward-only: a
/// desktop downgrade restores a backup rather than running a `down` step.
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(BASELINE_DDL), M::up(CONTRIBUTOR_INDEX_DDL)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn the_migration_set_is_well_formed() {
        migrations().validate().expect("migrations must validate");
    }

    #[test]
    fn applying_the_migrations_reaches_the_target_version() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations()
            .to_latest(&mut conn)
            .expect("migrations must apply");
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, TARGET_VERSION);
    }

    #[test]
    fn applying_the_migrations_twice_is_idempotent() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations().to_latest(&mut conn).expect("first apply");
        // Re-running against an already-migrated database is a no-op.
        migrations().to_latest(&mut conn).expect("second apply");
    }
}
