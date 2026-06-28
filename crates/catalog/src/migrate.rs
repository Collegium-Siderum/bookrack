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

pub(crate) const TARGET_VERSION: i64 = 10;

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

// `M[2]` — re-key the six node-curation tables from a bare physical
// `node_id` to the content-stable logical address `(intake_id, scope)`.
//
// The general procedure is SQLite's 12-step table rebuild
// (sqlite.org/lang_altertable), for when a future migration rebuilds a
// table that *does* carry foreign keys, triggers, or views:
//   1.  PRAGMA foreign_keys=OFF              (done once in from_connection)
//   2.  BEGIN                                (rusqlite_migration wraps each M)
//   3.  note dependent indexes/triggers/views
//   4.  CREATE TABLE <t>_new (... new shape ...)
//   5.  INSERT INTO <t>_new SELECT ... FROM <t>   <-- SKIPPED: tables empty
//   6.  DROP TABLE <t>
//   7.  ALTER TABLE <t>_new RENAME TO <t>
//   8.  recreate indexes / triggers / views
//   9.  recreate any views that referenced the table
//   10. PRAGMA foreign_key_check
//   11. COMMIT
//   12. PRAGMA foreign_keys=ON               (done once in from_connection)
//
// These six tables carry no foreign keys and are empty (METADATA is not
// yet live), so steps 5/9/10 drop out and each table reduces to 4/6/7/8.
const NODE_ADDR_DDL: &str = r#"
-- node_publication_attrs: single-column PK becomes composite.
CREATE TABLE node_publication_attrs_new (
  intake_id INTEGER NOT NULL,
  scope TEXT NOT NULL,
  title TEXT, subtitle TEXT, publisher TEXT, year TEXT,
  publication_date TEXT, isbn TEXT, series TEXT, series_number TEXT,
  edition TEXT, language TEXT, original_title TEXT, original_language TEXT,
  source_format TEXT, source TEXT, confidence TEXT, enriched_by TEXT,
  PRIMARY KEY (intake_id, scope)
);
DROP TABLE node_publication_attrs;
ALTER TABLE node_publication_attrs_new RENAME TO node_publication_attrs;

-- node_contributors: surrogate key kept; UNIQUE and covering index re-keyed.
CREATE TABLE node_contributors_new (
  contributor_id INTEGER PRIMARY KEY AUTOINCREMENT,
  intake_id INTEGER NOT NULL,
  scope TEXT NOT NULL,
  role TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  origin TEXT NOT NULL,
  name TEXT NOT NULL,
  nationality TEXT,
  inheritable INTEGER NOT NULL DEFAULT 1,
  UNIQUE (intake_id, scope, role, ordinal, origin)
);
DROP TABLE node_contributors;
ALTER TABLE node_contributors_new RENAME TO node_contributors;
-- Both indexes are recreated on the new shape; dropping the old table
-- took idx_contrib_role_name (baseline) and idx_contrib_node (M[1]) with it.
CREATE INDEX idx_contrib_role_name ON node_contributors(role, name);
CREATE INDEX idx_contrib_node ON node_contributors(intake_id, scope, role, ordinal);

-- node_overrides
CREATE TABLE node_overrides_new (
  intake_id INTEGER NOT NULL,
  scope TEXT NOT NULL,
  field TEXT NOT NULL,
  value TEXT,
  confirmed INTEGER NOT NULL DEFAULT 0,
  curated_at TEXT NOT NULL,
  curated_by TEXT NOT NULL,
  notes TEXT,
  PRIMARY KEY (intake_id, scope, field)
);
DROP TABLE node_overrides;
ALTER TABLE node_overrides_new RENAME TO node_overrides;

-- node_role_takeovers
CREATE TABLE node_role_takeovers_new (
  intake_id INTEGER NOT NULL,
  scope TEXT NOT NULL,
  role TEXT NOT NULL,
  curated_at TEXT NOT NULL,
  curated_by TEXT NOT NULL,
  notes TEXT,
  PRIMARY KEY (intake_id, scope, role)
);
DROP TABLE node_role_takeovers;
ALTER TABLE node_role_takeovers_new RENAME TO node_role_takeovers;

-- node_categories
CREATE TABLE node_categories_new (
  intake_id INTEGER NOT NULL,
  scope TEXT NOT NULL,
  category TEXT NOT NULL,
  is_primary INTEGER NOT NULL DEFAULT 0,
  source TEXT NOT NULL,
  confirmed INTEGER NOT NULL DEFAULT 0,
  curated_at TEXT NOT NULL,
  curated_by TEXT NOT NULL,
  PRIMARY KEY (intake_id, scope, category)
);
DROP TABLE node_categories;
ALTER TABLE node_categories_new RENAME TO node_categories;
CREATE INDEX idx_cat_cat ON node_categories(category);

-- node_reviews: single-column PK becomes composite.
CREATE TABLE node_reviews_new (
  intake_id INTEGER NOT NULL,
  scope TEXT NOT NULL,
  reviewed_at TEXT NOT NULL,
  reviewed_by TEXT NOT NULL,
  status TEXT NOT NULL,
  notes TEXT,
  PRIMARY KEY (intake_id, scope)
);
DROP TABLE node_reviews;
ALTER TABLE node_reviews_new RENAME TO node_reviews;
"#;

// `M[3]` — add two columns to `node_publication_attrs`:
//
//   * `pub_place`: city of publication, required by the GB/T 7714 and
//     Chicago bibliography styles.
//   * `original_year`: a translation's original-language publication
//     year, a pre-FRBR stopgap matching the existing `original_title` /
//     `original_language` columns.
//
// Pure additive: SQLite's `ALTER TABLE ... ADD COLUMN` is O(1) and
// leaves existing rows with NULL in the new columns.
const PUB_PLACE_ORIGINAL_YEAR_DDL: &str = "\
ALTER TABLE node_publication_attrs ADD COLUMN pub_place TEXT;\n\
ALTER TABLE node_publication_attrs ADD COLUMN original_year TEXT;\n";

// `M[4]` — collapse `intake.extractor_version` from a per-adapter string
// to a single integer carrying `bookrack_extract::EXTRACTOR_VERSION`. The
// string form had no production reader; existing rows back-fill to `1`,
// the initial value of the const, because no behaviour-sensitive change
// has happened yet from their perspective.
//
// SQLite cannot rewrite a column's type in place, so the table is
// rebuilt via the 12-step pattern. `intake` carries no foreign keys
// referencing it and no triggers/views, so steps 9/10 drop out; the
// surviving steps are 4 (CREATE), 5 (INSERT … SELECT), 6 (DROP),
// 7 (RENAME), 8 (recreate indexes), plus an explicit `sqlite_sequence`
// reset so AUTOINCREMENT keeps issuing fresh `intake_id` values past
// the highest pre-migration row.
const INTAKE_EXTRACTOR_VERSION_DDL: &str = r#"
CREATE TABLE intake_new (
  intake_id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_sha256 TEXT NOT NULL UNIQUE,
  stored_path TEXT,
  original_path TEXT,
  format TEXT,
  byte_size INTEGER,
  adapter TEXT,
  extractor_version INTEGER NOT NULL DEFAULT 1,
  intake_at TEXT NOT NULL,
  status TEXT NOT NULL,
  expression_id INTEGER,
  notes TEXT
);
INSERT INTO intake_new (
  intake_id, source_sha256, stored_path, original_path, format, byte_size,
  adapter, extractor_version, intake_at, status, expression_id, notes
)
SELECT
  intake_id, source_sha256, stored_path, original_path, format, byte_size,
  adapter, 1, intake_at, status, expression_id, notes
FROM intake;
DROP TABLE intake;
ALTER TABLE intake_new RENAME TO intake;
INSERT OR REPLACE INTO sqlite_sequence (name, seq)
  SELECT 'intake', COALESCE(MAX(intake_id), 0) FROM intake;
CREATE INDEX idx_intake_status ON intake(status);
CREATE INDEX idx_intake_format ON intake(format);
"#;

// `M[5]` — stamp the audit verdict on the publication-attrs row
// alongside the existing `confidence` column, so `metadata list` and
// `metadata show` agree on the historical audit outcome instead of one
// reading the stored row and the other re-running a synthetic audit.
// Additive: `ALTER TABLE ... ADD COLUMN` is O(1) and leaves existing
// rows with `NULL` until the next ingest restamps them.
const AUDIT_VERDICT_DDL: &str = "ALTER TABLE node_publication_attrs ADD COLUMN audit_verdict TEXT;";

// `M[6]` — add `intake.page_count` for paginated sources: physical
// sheet count of a PDF, a TIFF stack, an image folder, or an OCR
// product. The column is nullable: reflow formats (EPUB / HTML / TXT)
// carry no page count, and rows registered before the column existed
// read back as NULL. Additive: `ALTER TABLE ... ADD COLUMN` is O(1)
// and leaves existing rows with NULL.
const INTAKE_PAGE_COUNT_DDL: &str = "ALTER TABLE intake ADD COLUMN page_count INTEGER;";

// `M[7]` — generalize the curation tables for both pipelines:
//
//   * Rename `book_state` -> `item_state` and `book_pipeline_audit` ->
//     `item_pipeline_audit`. The composite key `book_root_id`/`scope`
//     stays book-shaped on the wire; the rename only generalizes the
//     containers so the glean pipeline can land paper rows alongside
//     ingest's book rows without a parallel table.
//   * Add the seven discrete bibliographic columns the paper pipeline
//     needs on `node_publication_attrs` (DOI, arXiv id, ISSN, container
//     title, abstract text, CSL type, plus an `extras_json` blob for
//     anything CSL preserves but no discrete column captures).
//   * Add the three contributor columns CSL-JSON consumes when carrying
//     a structured `Name`: `family`, `given`, and `orcid`.
//
// All additive columns are nullable; existing rows backfill to NULL,
// and book ingest leaves them at NULL. The index pair on the old
// `book_state` table is dropped and re-issued under the new prefix so
// the spec's `IndexSpec` names match what is on disk; `idx_pa_*` on
// `item_pipeline_audit` keep their (table-agnostic) names and follow
// the renamed table automatically.
const ITEM_STATE_AND_PAPER_COLUMNS_DDL: &str = r#"
ALTER TABLE book_state RENAME TO item_state;
DROP INDEX idx_book_state_stage;
DROP INDEX idx_book_state_embed;
CREATE INDEX idx_item_state_stage ON item_state(current_stage);
CREATE INDEX idx_item_state_embed ON item_state(embedded_at) WHERE embedded_at IS NULL;

ALTER TABLE book_pipeline_audit RENAME TO item_pipeline_audit;

ALTER TABLE node_publication_attrs ADD COLUMN doi TEXT;
ALTER TABLE node_publication_attrs ADD COLUMN arxiv_id TEXT;
ALTER TABLE node_publication_attrs ADD COLUMN issn TEXT;
ALTER TABLE node_publication_attrs ADD COLUMN container_title TEXT;
ALTER TABLE node_publication_attrs ADD COLUMN abstract_text TEXT;
ALTER TABLE node_publication_attrs ADD COLUMN csl_type TEXT;
ALTER TABLE node_publication_attrs ADD COLUMN extras_json TEXT;

ALTER TABLE node_contributors ADD COLUMN family TEXT;
ALTER TABLE node_contributors ADD COLUMN given TEXT;
ALTER TABLE node_contributors ADD COLUMN orcid TEXT;
"#;

// `M[8]` — record the absolute path of the source PDF's byte archive on
// the intake row. The paper pipeline copies the source file into
// `papers_dir/paper-{intake_id}.pdf` alongside the envelope and writes
// the canonical path here, so downstream tools (raster render, fetch,
// forensic re-open) can locate the original bytes without scanning the
// directory or recomputing a SHA. Existing rows backfill to NULL; the
// `stored_path` column continues to point at the envelope, leaving the
// two pointers orthogonal.
const INTAKE_SOURCE_PDF_PATH_DDL: &str = "ALTER TABLE intake ADD COLUMN source_pdf_path TEXT;";

// `M[9]` — add the distill audit pair: `book_distill_audit`, one row
// per distill build of one reference book, plus
// `book_distill_stage_report`, one row per pipeline stage tied to its
// header by `run_id`. Both tables ship together so the audit and its
// per-stage breakdown reach v10 atomically; downstream rollups address
// rows by `(run_id, ord)` from this point. Additive: no existing table
// is touched.
const BOOK_DISTILL_AUDIT_DDL: &str = r#"
CREATE TABLE book_distill_audit (
  run_id INTEGER PRIMARY KEY AUTOINCREMENT,
  book_slug TEXT NOT NULL,
  source_path TEXT NOT NULL,
  started_at TEXT NOT NULL,
  finished_at TEXT NOT NULL,
  pages INTEGER NOT NULL,
  blocks INTEGER NOT NULL,
  raws INTEGER NOT NULL,
  splits INTEGER NOT NULL,
  entries INTEGER NOT NULL,
  unmatched_lines INTEGER NOT NULL,
  pair_mismatch INTEGER NOT NULL,
  gate_status TEXT NOT NULL CHECK (gate_status IN ('pass', 'fail', 'off')),
  gate_threshold REAL,
  profile_ref TEXT NOT NULL DEFAULT '',
  extractor_version TEXT NOT NULL
);
CREATE INDEX idx_book_distill_audit_slug_time
  ON book_distill_audit(book_slug, started_at);

CREATE TABLE book_distill_stage_report (
  run_id INTEGER NOT NULL REFERENCES book_distill_audit(run_id) ON DELETE CASCADE,
  ord INTEGER NOT NULL,
  stage_name TEXT NOT NULL,
  in_kind TEXT NOT NULL,
  out_kind TEXT NOT NULL,
  in_len INTEGER NOT NULL,
  out_len INTEGER NOT NULL,
  PRIMARY KEY (run_id, ord)
);
CREATE INDEX idx_book_distill_stage_report_stage
  ON book_distill_stage_report(stage_name);
"#;

/// The migration sequence applied to `catalog.db` on open. Forward-only: a
/// desktop downgrade restores a backup rather than running a `down` step.
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(BASELINE_DDL),
        M::up(CONTRIBUTOR_INDEX_DDL),
        M::up(NODE_ADDR_DDL),
        M::up(PUB_PLACE_ORIGINAL_YEAR_DDL),
        M::up(INTAKE_EXTRACTOR_VERSION_DDL),
        M::up(AUDIT_VERDICT_DDL),
        M::up(INTAKE_PAGE_COUNT_DDL),
        M::up(ITEM_STATE_AND_PAPER_COLUMNS_DDL),
        M::up(INTAKE_SOURCE_PDF_PATH_DDL),
        M::up(BOOK_DISTILL_AUDIT_DDL),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, named_params};

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

    /// The set of column names on `table`.
    fn columns_of(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("prepare table_info");
        stmt.query_map([], |row| row.get::<_, String>("name"))
            .expect("query")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("collect")
    }

    /// Whether an index of `name` exists.
    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = :name",
            named_params! { ":name": name },
            |row| row.get::<_, i64>(0),
        )
        .expect("query index")
            > 0
    }

    /// The type of column `column` on `table`, as SQLite reports it.
    fn column_type(conn: &Connection, table: &str, column: &str) -> String {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("prepare table_info");
        let row = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>("name")?, row.get::<_, String>("type")?))
            })
            .expect("query")
            .map(|r| r.expect("row"))
            .find(|(name, _)| name == column)
            .unwrap_or_else(|| panic!("{table}.{column} missing"));
        row.1
    }

    #[test]
    fn migration_m4_rebuilds_intake_with_an_integer_extractor_version() {
        let mut conn = Connection::open_in_memory().expect("open");
        // Stop one short of M[4] and seed a row that carries the legacy
        // TEXT extractor_version, so the migration's INSERT … SELECT has
        // a row to backfill.
        migrations()
            .to_version(&mut conn, 4)
            .expect("apply M[0..3]");
        conn.execute(
            "INSERT INTO intake (\
               source_sha256, original_path, format, byte_size, \
               adapter, extractor_version, intake_at, status\
             ) VALUES ('sha-rt', '/tmp/book.epub', 'epub', 8192, \
                       'epub', 'rbook=0.7;scraper=0.27;epub-adapter=1', \
                       '2026-06-04T00:00:00Z', 'extracted')",
            [],
        )
        .expect("seed legacy row");
        let legacy_id: i64 = conn
            .query_row("SELECT intake_id FROM intake", [], |row| row.get(0))
            .expect("read legacy id");

        migrations().to_latest(&mut conn).expect("apply M[4]");

        assert_eq!(column_type(&conn, "intake", "extractor_version"), "INTEGER");
        let (id, ev): (i64, i64) = conn
            .query_row(
                "SELECT intake_id, extractor_version FROM intake",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read migrated row");
        assert_eq!(id, legacy_id, "intake_id survives the rebuild");
        assert_eq!(ev, 1, "extractor_version backfills to 1");
        assert!(
            index_exists(&conn, "idx_intake_status"),
            "idx_intake_status recreated"
        );
        assert!(
            index_exists(&conn, "idx_intake_format"),
            "idx_intake_format recreated"
        );

        // A fresh insert via the standard path must receive an id past
        // the highest pre-migration row, proving sqlite_sequence was
        // restored after the rebuild.
        conn.execute(
            "INSERT INTO intake (source_sha256, intake_at, status) \
             VALUES ('sha-next', '2026-06-04T00:00:01Z', 'pending')",
            [],
        )
        .expect("insert next row");
        let next_id: i64 = conn
            .query_row(
                "SELECT intake_id FROM intake WHERE source_sha256 = 'sha-next'",
                [],
                |row| row.get(0),
            )
            .expect("read next id");
        assert!(
            next_id > legacy_id,
            "next intake_id ({next_id}) must exceed the legacy id ({legacy_id})"
        );
    }

    #[test]
    fn migration_m3_adds_pub_place_and_original_year_to_publication_attrs() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations().to_latest(&mut conn).expect("apply");
        let cols = columns_of(&conn, "node_publication_attrs");
        assert!(
            cols.iter().any(|c| c == "pub_place"),
            "expected pub_place column, got {cols:?}"
        );
        assert!(
            cols.iter().any(|c| c == "original_year"),
            "expected original_year column, got {cols:?}"
        );
    }

    #[test]
    fn migration_m5_adds_audit_verdict_to_publication_attrs() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations().to_latest(&mut conn).expect("apply");
        let cols = columns_of(&conn, "node_publication_attrs");
        assert!(
            cols.iter().any(|c| c == "audit_verdict"),
            "expected audit_verdict column, got {cols:?}"
        );
    }

    #[test]
    fn migration_m6_adds_page_count_to_intake_as_nullable_integer() {
        let mut conn = Connection::open_in_memory().expect("open");
        // Stop one short of M[6] and seed a pre-migration row so the
        // post-migration NULL backfill can be asserted explicitly.
        migrations()
            .to_version(&mut conn, 6)
            .expect("apply M[0..5]");
        conn.execute(
            "INSERT INTO intake (\
               source_sha256, original_path, format, byte_size, \
               adapter, extractor_version, intake_at, status\
             ) VALUES ('sha-legacy', '/tmp/book.pdf', 'pdf', 8192, \
                       'pdf', 1, '2026-06-05T00:00:00Z', 'extracted')",
            [],
        )
        .expect("seed pre-M[6] row");

        migrations().to_latest(&mut conn).expect("apply M[6]");

        let cols = columns_of(&conn, "intake");
        assert!(
            cols.iter().any(|c| c == "page_count"),
            "expected page_count column, got {cols:?}"
        );
        assert_eq!(column_type(&conn, "intake", "page_count"), "INTEGER");

        // Pre-migration row reads back NULL on the new column.
        let legacy_pc: Option<i64> = conn
            .query_row(
                "SELECT page_count FROM intake WHERE source_sha256 = 'sha-legacy'",
                [],
                |row| row.get(0),
            )
            .expect("read legacy row");
        assert_eq!(legacy_pc, None);
    }

    #[test]
    fn migration_m7_renames_item_tables_and_adds_paper_columns() {
        let mut conn = Connection::open_in_memory().expect("open");
        // Stop one short of M[7] so the pre-migration shape (old table
        // names, no paper columns) can be seeded and then asserted to
        // have moved correctly.
        migrations()
            .to_version(&mut conn, 7)
            .expect("apply M[0..6]");

        // Seed one row on each of the two soon-to-be-renamed tables and
        // one publication_attrs row to verify backfill onto the new
        // columns later.
        conn.execute(
            "INSERT INTO intake (\
               source_sha256, original_path, format, byte_size, \
               adapter, extractor_version, intake_at, status\
             ) VALUES ('sha-legacy', '/tmp/book.pdf', 'pdf', 8192, \
                       'pdf', 1, '2026-06-05T00:00:00Z', 'extracted')",
            [],
        )
        .expect("seed intake row");
        conn.execute(
            "INSERT INTO book_state (\
               book_root_id, intake_id, current_stage\
             ) VALUES (10, 1, 'extracted')",
            [],
        )
        .expect("seed book_state row");
        conn.execute(
            "INSERT INTO book_pipeline_audit (\
               book_root_id, source_sha256, stage, sub_step, outcome, \
               ts, pipeline_run_id, actor_kind\
             ) VALUES (10, 'sha-legacy', 'extract', 'parse', 'ok', \
                       '2026-06-05T00:00:00Z', 'run-1', 'pipeline')",
            [],
        )
        .expect("seed book_pipeline_audit row");
        conn.execute(
            "INSERT INTO node_publication_attrs (\
               intake_id, scope, title\
             ) VALUES (1, 'book', 'Legacy Title')",
            [],
        )
        .expect("seed publication_attrs row");

        migrations().to_latest(&mut conn).expect("apply M[7]");

        // book_state and book_pipeline_audit no longer exist by their
        // old names; the renamed tables carry the seeded rows forward.
        assert!(
            columns_of(&conn, "book_state").is_empty(),
            "book_state should be gone after the rename"
        );
        assert!(
            columns_of(&conn, "book_pipeline_audit").is_empty(),
            "book_pipeline_audit should be gone after the rename"
        );
        let state_stage: String = conn
            .query_row(
                "SELECT current_stage FROM item_state WHERE book_root_id = 10",
                [],
                |row| row.get(0),
            )
            .expect("read item_state row");
        assert_eq!(state_stage, "extracted");
        let audit_stage: String = conn
            .query_row(
                "SELECT stage FROM item_pipeline_audit WHERE book_root_id = 10",
                [],
                |row| row.get(0),
            )
            .expect("read item_pipeline_audit row");
        assert_eq!(audit_stage, "extract");

        // The paired indexes on item_state were dropped and reissued
        // under the new prefix so the spec's IndexSpec names match the
        // live database.
        assert!(index_exists(&conn, "idx_item_state_stage"));
        assert!(index_exists(&conn, "idx_item_state_embed"));
        assert!(!index_exists(&conn, "idx_book_state_stage"));
        assert!(!index_exists(&conn, "idx_book_state_embed"));

        // Paper-side discrete columns on node_publication_attrs.
        let attrs_cols = columns_of(&conn, "node_publication_attrs");
        for col in [
            "doi",
            "arxiv_id",
            "issn",
            "container_title",
            "abstract_text",
            "csl_type",
            "extras_json",
        ] {
            assert!(
                attrs_cols.iter().any(|c| c == col),
                "expected {col} column on node_publication_attrs, got {attrs_cols:?}"
            );
            assert_eq!(column_type(&conn, "node_publication_attrs", col), "TEXT");
        }
        // The seeded book row reads back NULL on each new column.
        let legacy_doi: Option<String> = conn
            .query_row(
                "SELECT doi FROM node_publication_attrs \
                 WHERE intake_id = 1 AND scope = 'book'",
                [],
                |row| row.get(0),
            )
            .expect("read legacy attrs row");
        assert_eq!(legacy_doi, None);

        // CSL-JSON structured-name columns on node_contributors.
        let contrib_cols = columns_of(&conn, "node_contributors");
        for col in ["family", "given", "orcid"] {
            assert!(
                contrib_cols.iter().any(|c| c == col),
                "expected {col} column on node_contributors, got {contrib_cols:?}"
            );
            assert_eq!(column_type(&conn, "node_contributors", col), "TEXT");
        }
    }

    #[test]
    fn migration_m9_adds_the_distill_audit_pair_with_their_indexes() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations()
            .to_version(&mut conn, 9)
            .expect("apply M[0..8]");
        // The pre-migration database carries neither table.
        assert!(columns_of(&conn, "book_distill_audit").is_empty());
        assert!(columns_of(&conn, "book_distill_stage_report").is_empty());

        migrations().to_latest(&mut conn).expect("apply M[9]");

        let header_cols = columns_of(&conn, "book_distill_audit");
        for col in [
            "run_id",
            "book_slug",
            "source_path",
            "started_at",
            "finished_at",
            "pages",
            "blocks",
            "raws",
            "splits",
            "entries",
            "unmatched_lines",
            "pair_mismatch",
            "gate_status",
            "gate_threshold",
            "profile_ref",
            "extractor_version",
        ] {
            assert!(
                header_cols.iter().any(|c| c == col),
                "expected {col} on book_distill_audit, got {header_cols:?}"
            );
        }
        let stage_cols = columns_of(&conn, "book_distill_stage_report");
        for col in [
            "run_id",
            "ord",
            "stage_name",
            "in_kind",
            "out_kind",
            "in_len",
            "out_len",
        ] {
            assert!(
                stage_cols.iter().any(|c| c == col),
                "expected {col} on book_distill_stage_report, got {stage_cols:?}"
            );
        }
        assert!(index_exists(&conn, "idx_book_distill_audit_slug_time"));
        assert!(index_exists(&conn, "idx_book_distill_stage_report_stage"));
    }

    #[test]
    fn migration_m8_adds_source_pdf_path_to_intake_as_nullable_text() {
        let mut conn = Connection::open_in_memory().expect("open");
        // Stop one short of M[8] and seed a pre-migration intake row so
        // the post-migration NULL backfill can be asserted explicitly.
        migrations()
            .to_version(&mut conn, 8)
            .expect("apply M[0..7]");
        conn.execute(
            "INSERT INTO intake (\
               source_sha256, original_path, format, byte_size, \
               adapter, extractor_version, intake_at, status\
             ) VALUES ('sha-legacy', '/tmp/paper.pdf', 'pdf', 8192, \
                       'pdf', 1, '2026-06-13T00:00:00Z', 'extracted')",
            [],
        )
        .expect("seed pre-M[8] row");

        migrations().to_latest(&mut conn).expect("apply M[8]");

        let cols = columns_of(&conn, "intake");
        assert!(
            cols.iter().any(|c| c == "source_pdf_path"),
            "expected source_pdf_path column, got {cols:?}"
        );
        assert_eq!(column_type(&conn, "intake", "source_pdf_path"), "TEXT");

        // Pre-migration row reads back NULL on the new column.
        let legacy_path: Option<String> = conn
            .query_row(
                "SELECT source_pdf_path FROM intake WHERE source_sha256 = 'sha-legacy'",
                [],
                |row| row.get(0),
            )
            .expect("read legacy row");
        assert_eq!(legacy_path, None);
    }

    #[test]
    fn the_address_migration_rekeys_every_node_table() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations().to_latest(&mut conn).expect("apply");

        // Each node-curation table now carries the logical address and no
        // longer the bare physical node id.
        for table in [
            "node_publication_attrs",
            "node_contributors",
            "node_overrides",
            "node_role_takeovers",
            "node_categories",
            "node_reviews",
        ] {
            let cols = columns_of(&conn, table);
            assert!(
                cols.iter().any(|c| c == "intake_id"),
                "{table} keeps intake_id"
            );
            assert!(cols.iter().any(|c| c == "scope"), "{table} keeps scope");
            assert!(
                !cols.iter().any(|c| c == "node_id"),
                "{table} drops node_id"
            );
        }

        // The contributor indexes and the category index survive the
        // rebuild that dropped the tables they hung on.
        assert!(index_exists(&conn, "idx_contrib_node"));
        assert!(index_exists(&conn, "idx_contrib_role_name"));
        assert!(index_exists(&conn, "idx_cat_cat"));
    }
}
