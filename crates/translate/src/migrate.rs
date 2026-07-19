// SPDX-License-Identifier: Apache-2.0

//! The `translate.db` schema migration sequence.
//!
//! `translate.db` is a source of truth: sealed translations and
//! glossary decisions cannot be rebuilt from any other store, so every
//! schema change must carry existing rows forward. The applied revision
//! lives in SQLite's `user_version`, advanced by `rusqlite_migration`.
//!
//! `M[0]` is the frozen baseline: the seven tables and four indexes of
//! the v1 schema, rendered once from the per-table specs. The baseline
//! text is never edited; later schema changes append their own
//! migrations. The specs stay the single source of truth — `verify_all`
//! checks the live schema against them on every open, so a baseline
//! that drifted from the specs fails loudly.

use rusqlite_migration::{M, Migrations};

/// The target `user_version` of `translate.db`. Bumps in lockstep with
/// the migration vector.
pub const TARGET_VERSION: i64 = 1;

/// `M[0]` — the frozen baseline schema. Immutable: never edit this
/// text; add a new migration instead.
const BASELINE_DDL: &str = r#"
-- Immutable translation units mirroring corpus structure.
CREATE TABLE IF NOT EXISTS translate_units (
  unit_id INTEGER PRIMARY KEY,
  intake_id INTEGER NOT NULL,  -- soft reference to the catalog intake; no cascade
  target_lang TEXT NOT NULL,
  node_id INTEGER NOT NULL,  -- soft reference to the corpus node; re-anchored via source_outline
  unit_order INTEGER NOT NULL,
  source_outline TEXT,  -- chapter-path snapshot; drives re-anchoring and TOC backfill
  injection_profile TEXT NOT NULL DEFAULT 'default',
  UNIQUE (intake_id, target_lang, node_id)
);
CREATE INDEX IF NOT EXISTS unit_by_intake ON translate_units(intake_id, target_lang, unit_order);

-- Mutable translation segments; the unit of translation work.
CREATE TABLE IF NOT EXISTS translate_segments (
  segment_id INTEGER PRIMARY KEY,
  unit_id INTEGER NOT NULL REFERENCES translate_units(unit_id),
  start_node_id INTEGER NOT NULL,  -- soft reference to the corpus node the span starts in
  start_char_offset INTEGER NOT NULL,
  end_node_id INTEGER NOT NULL,  -- soft reference to the corpus node the span ends in
  end_char_offset INTEGER NOT NULL,
  source_text_sha TEXT NOT NULL,  -- content fingerprint; drift sentinel and re-anchor key
  status TEXT NOT NULL CHECK (status IN ('draft', 'proposed', 'sealed')),
  draft_text TEXT,
  reflection_notes TEXT,  -- JSON; reflection or review-note payload
  final_text TEXT,  -- semantically locked form; other formats derive at export
  source_kind TEXT CHECK (source_kind IN ('human', 'llm-draft', 'llm-reflected', 'edited', 'imported')),
  sealed_at TEXT,
  version INTEGER NOT NULL DEFAULT 1,
  UNIQUE (unit_id, start_node_id, start_char_offset, end_node_id, end_char_offset)
);
CREATE INDEX IF NOT EXISTS seg_by_unit ON translate_segments(unit_id, start_char_offset);
CREATE INDEX IF NOT EXISTS seg_by_status ON translate_segments(status, sealed_at);

-- Glossary concept layer: one row per tracked source term.
CREATE TABLE IF NOT EXISTS glossary_terms (
  term_id INTEGER PRIMARY KEY,
  scope TEXT NOT NULL CHECK (scope IN ('authority', 'library', 'book')),
  scope_ref TEXT,  -- book: intake id; authority: refs book slug; library: NULL
  source_lang TEXT NOT NULL,
  source_term TEXT NOT NULL,
  source_norm TEXT NOT NULL,
  term_kind TEXT NOT NULL CHECK (term_kind IN ('term', 'proper_noun', 'do_not_translate', 'common_knowledge')),
  primary_choice_id INTEGER,  -- glossary_translations id; no FK, the write path validates
  UNIQUE (source_lang, source_norm, scope, scope_ref)
);

-- Candidate renderings of glossary terms; superseded rows stay.
CREATE TABLE IF NOT EXISTS glossary_translations (
  translation_id INTEGER PRIMARY KEY,
  term_id INTEGER NOT NULL REFERENCES glossary_terms(term_id),
  target_lang TEXT NOT NULL,
  target_term TEXT,  -- NULL records a do-not-translate verdict
  faction TEXT,
  translator TEXT,
  citation TEXT,
  rationale TEXT,
  status TEXT NOT NULL CHECK (status IN ('candidate', 'active', 'retired', 'rejected')),
  authority_ref TEXT,  -- refs://<book_slug>#<entry_key> URI; library-relative soft reference
  proposed_at TEXT NOT NULL,
  approved_at TEXT,
  version INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS gt_by_term ON glossary_translations(term_id, target_lang, status);

-- Append-only audit of translation actions; a recording, not the state machine.
CREATE TABLE IF NOT EXISTS translate_audit (
  audit_id INTEGER PRIMARY KEY,
  segment_id INTEGER,  -- subject: at most one of the three id columns is set
  term_id INTEGER,
  translation_id INTEGER,
  action TEXT NOT NULL,
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')),
  actor_detail TEXT,
  session_id TEXT,
  reason TEXT,
  payload_json TEXT,  -- snapshot of the action's inputs and outputs
  cost_tokens INTEGER,  -- bare numeric so budget queries can SUM
  changed_at TEXT NOT NULL
);

-- Witness texts anchored per unit; chapter-to-chapter alignment.
CREATE TABLE IF NOT EXISTS translate_unit_witnesses (
  witness_id INTEGER PRIMARY KEY,
  unit_id INTEGER NOT NULL REFERENCES translate_units(unit_id),
  witness_intake_id INTEGER NOT NULL,  -- soft reference to the catalog intake; no cascade
  witness_node_id INTEGER NOT NULL,
  lang TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('alt_source', 'translation_witness', 'prior_translation')),
  note TEXT,  -- free-form witness credentials
  UNIQUE (unit_id, witness_intake_id)
);

-- Key/value scalars: schema-version mirror and reader-version stamp.
CREATE TABLE IF NOT EXISTS translate_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

/// The migration sequence applied to `translate.db` on open.
pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(BASELINE_DDL)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migrations_validate() {
        migrations().validate().expect("migrations must validate");
    }

    #[test]
    fn baseline_creates_every_expected_object() {
        let mut conn = Connection::open_in_memory().expect("open in memory");
        migrations().to_latest(&mut conn).expect("apply");

        let object_exists = |object_type: &str, name: &str| -> bool {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                    rusqlite::params![object_type, name],
                    |row| row.get(0),
                )
                .expect("query sqlite_master");
            count > 0
        };

        for table in [
            "translate_units",
            "translate_segments",
            "glossary_terms",
            "glossary_translations",
            "translate_audit",
            "translate_unit_witnesses",
            "translate_meta",
        ] {
            assert!(object_exists("table", table), "expected table {table}");
        }
        for index in [
            "unit_by_intake",
            "seg_by_unit",
            "seg_by_status",
            "gt_by_term",
        ] {
            assert!(object_exists("index", index), "expected index {index}");
        }
    }
}
