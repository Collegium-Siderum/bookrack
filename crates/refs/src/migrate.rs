// SPDX-License-Identifier: Apache-2.0

//! The `reference.db` schema migration sequence.
//!
//! `reference.db` can be rebuilt from the source OCR product and
//! `book.toml` for any reference book, but the user-authored overlay
//! layer cannot: any schema change must therefore preserve overlay rows
//! across upgrades. The applied revision lives in SQLite's `user_version`,
//! advanced by `rusqlite_migration`.
//!
//! `M[0]` is the frozen baseline: the `reference_books`,
//! `reference_entries`, and `reference_entry_overlays` tables, the
//! `reference_entries_resolved` view that merges base and overlay through
//! `json_patch`, the `reference_entries_fts` trigram sidecar, and the
//! three triggers that keep that sidecar synchronised with the base
//! table. Later changes layer on as their own migrations and never edit
//! the baseline text.

use rusqlite_migration::{M, Migrations};

/// The target `user_version` of `reference.db`. Bumps in lockstep with
/// the migration vector.
pub const TARGET_VERSION: i64 = 1;

/// `M[0]` — the frozen baseline schema. Immutable: never edit this text;
/// add a new migration instead.
const BASELINE_DDL: &str = r#"
-- Per-book registry: one row per ingested reference book. `intake_id`
-- is a soft cross-database reference to the `catalog.intake` row that
-- holds the source file's identity; SQLite cannot enforce foreign keys
-- across attached databases, so this column carries the id without a
-- REFERENCES clause.
CREATE TABLE reference_books (
    book_slug         TEXT PRIMARY KEY,
    schema_name       TEXT NOT NULL,
    schema_version    INTEGER NOT NULL,
    parser_version    TEXT NOT NULL,
    title_zh          TEXT NOT NULL,
    title_en          TEXT,
    edition           TEXT,
    publisher         TEXT,
    year              INTEGER,
    isbn              TEXT,
    authority_rank    INTEGER NOT NULL DEFAULT 0,
    built_at          TEXT NOT NULL,
    intake_id         INTEGER,
    entry_count       INTEGER NOT NULL DEFAULT 0,
    parse_warnings    INTEGER NOT NULL DEFAULT 0
);

-- Distilled entries from every reference book. The base layer: distill
-- rebuilds rewrite this table in full. The stable external handle is
-- the composite `(book_slug, entry_key)`; `entry_id` is an internal
-- join / FTS5 rowid bridge that may shift across rebuilds and must not
-- appear in MCP responses, book.toml references, or export formats.
CREATE TABLE reference_entries (
    entry_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    book_slug         TEXT NOT NULL REFERENCES reference_books(book_slug) ON DELETE CASCADE,
    entry_key         TEXT NOT NULL,
    headword          TEXT NOT NULL,
    aliases_json      TEXT,
    payload_json      TEXT NOT NULL,
    fts_text          TEXT NOT NULL,
    source_json       TEXT NOT NULL,
    quality_flags     TEXT,
    UNIQUE (book_slug, entry_key)
);

CREATE INDEX ix_ref_entries_book_key ON reference_entries(book_slug, entry_key);

-- User edits layered on top of `reference_entries`. One row per
-- composite key carries the patch JSON, an optional quality_flags
-- override, the base row's `built_at` at the time of edit (so a later
-- base rebuild can flag drift), and a free-text edit reason. distill
-- rebuilds the base layer without touching this table.
CREATE TABLE reference_entry_overlays (
    book_slug         TEXT NOT NULL,
    entry_key         TEXT NOT NULL,
    overlay_json      TEXT NOT NULL,
    quality_flags     TEXT,
    base_built_at     TEXT,
    edit_reason       TEXT,
    edited_at         TEXT NOT NULL,
    PRIMARY KEY (book_slug, entry_key)
);

-- Read-side projection that merges base and overlay through json_patch.
-- `reference_lookup` always queries this view; overlay presence is
-- exposed to the caller through `has_overlay` but is otherwise
-- transparent. An overlay whose `(book_slug, entry_key)` no longer
-- exists in the base table simply drops out of the LEFT JOIN.
CREATE VIEW reference_entries_resolved AS
SELECT
    e.book_slug,
    e.entry_key,
    e.headword,
    e.aliases_json,
    json_patch(e.payload_json, COALESCE(o.overlay_json, '{}')) AS payload_json,
    COALESCE(o.quality_flags, e.quality_flags)               AS quality_flags,
    e.source_json,
    o.edit_reason,
    o.edited_at,
    (o.book_slug IS NOT NULL)                                 AS has_overlay
  FROM reference_entries e
  LEFT JOIN reference_entry_overlays o
    USING (book_slug, entry_key);

-- FTS5 trigram sidecar over the base table. `content_rowid='entry_id'`
-- pins the FTS rowid to the base row's autoincrement id; the three
-- triggers below keep the sidecar in sync. The trigram tokenizer is
-- the same one dbkit's conformance test verifies the bundled SQLite
-- ships with.
CREATE VIRTUAL TABLE reference_entries_fts USING fts5(
    headword,
    aliases_json,
    fts_text,
    content='reference_entries',
    content_rowid='entry_id',
    tokenize='trigram'
);

CREATE TRIGGER reference_entries_ai AFTER INSERT ON reference_entries BEGIN
  INSERT INTO reference_entries_fts(rowid, headword, aliases_json, fts_text)
  VALUES (new.entry_id, new.headword, new.aliases_json, new.fts_text);
END;

CREATE TRIGGER reference_entries_ad AFTER DELETE ON reference_entries BEGIN
  INSERT INTO reference_entries_fts(reference_entries_fts, rowid, headword, aliases_json, fts_text)
  VALUES('delete', old.entry_id, old.headword, old.aliases_json, old.fts_text);
END;

CREATE TRIGGER reference_entries_au AFTER UPDATE ON reference_entries BEGIN
  INSERT INTO reference_entries_fts(reference_entries_fts, rowid, headword, aliases_json, fts_text)
  VALUES('delete', old.entry_id, old.headword, old.aliases_json, old.fts_text);
  INSERT INTO reference_entries_fts(rowid, headword, aliases_json, fts_text)
  VALUES (new.entry_id, new.headword, new.aliases_json, new.fts_text);
END;
"#;

/// The migration sequence applied to `reference.db` on open.
pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(BASELINE_DDL)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_and_migrate() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in memory");
        migrations()
            .to_latest(&mut conn)
            .expect("migrations must apply");
        conn
    }

    fn object_exists(conn: &Connection, object_type: &str, name: &str) -> bool {
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                rusqlite::params![object_type, name],
                |row| row.get(0),
            )
            .expect("query sqlite_master");
        count > 0
    }

    #[test]
    fn migrations_validate() {
        migrations().validate().expect("migrations must validate");
    }

    #[test]
    fn applying_reaches_target_version() {
        let conn = open_and_migrate();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, TARGET_VERSION);
    }

    #[test]
    fn applying_twice_is_idempotent() {
        let mut conn = Connection::open_in_memory().expect("open");
        migrations().to_latest(&mut conn).expect("first apply");
        migrations().to_latest(&mut conn).expect("second apply");
    }

    #[test]
    fn baseline_creates_every_expected_object() {
        let conn = open_and_migrate();

        // Three user tables plus the FTS5 virtual table all appear as
        // `type='table'` in sqlite_master.
        for table in [
            "reference_books",
            "reference_entries",
            "reference_entry_overlays",
            "reference_entries_fts",
        ] {
            assert!(
                object_exists(&conn, "table", table),
                "expected table {table} to exist"
            );
        }

        assert!(
            object_exists(&conn, "view", "reference_entries_resolved"),
            "expected resolved view to exist"
        );

        for trigger in [
            "reference_entries_ai",
            "reference_entries_ad",
            "reference_entries_au",
        ] {
            assert!(
                object_exists(&conn, "trigger", trigger),
                "expected trigger {trigger} to exist"
            );
        }

        // The trigram tokenizer choice is part of the FTS5 sidecar's
        // contract with the catalog of book pipelines, so it travels in
        // the CREATE statement that sqlite_master keeps.
        let fts_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master \
                 WHERE type = 'table' AND name = 'reference_entries_fts'",
                [],
                |row| row.get(0),
            )
            .expect("read fts ddl");
        assert!(
            fts_sql.to_ascii_lowercase().contains("fts5"),
            "fts virtual table must use fts5: {fts_sql}"
        );
        assert!(
            fts_sql.to_ascii_lowercase().contains("trigram"),
            "fts virtual table must use the trigram tokenizer: {fts_sql}"
        );
    }

    /// Seed one book and two entries so overlay-resolution tests can
    /// share fixture data.
    fn seed_two_entries(conn: &Connection) {
        conn.execute(
            "INSERT INTO reference_books (\
               book_slug, schema_name, schema_version, parser_version, \
               title_zh, authority_rank, built_at\
             ) VALUES ('book_a', 'name_translation', 1, '0.1.0', \
                       'Book A', 10, '2026-06-25T00:00:00Z')",
            [],
        )
        .expect("seed book");
        conn.execute(
            "INSERT INTO reference_entries (\
               book_slug, entry_key, headword, aliases_json, payload_json, \
               fts_text, source_json, quality_flags\
             ) VALUES ('book_a', 'smith', 'Smith', '[\"Smyth\"]', \
                       '{\"country\":\"USA\",\"year\":1900}', \
                       'Smith Smyth alexander', \
                       '{\"book_slug\":\"book_a\",\"page\":1,\"sheet\":1,\
                          \"distill_run_id\":\"2026-06-25T00:00:00Z\"}', \
                       '[\"spliced_from_orphan\"]')",
            [],
        )
        .expect("seed entry smith");
        conn.execute(
            "INSERT INTO reference_entries (\
               book_slug, entry_key, headword, aliases_json, payload_json, \
               fts_text, source_json, quality_flags\
             ) VALUES ('book_a', 'jones', 'Jones', NULL, \
                       '{\"country\":\"UK\"}', \
                       'Jones canterbury', \
                       '{\"book_slug\":\"book_a\",\"page\":2,\"sheet\":2,\
                          \"distill_run_id\":\"2026-06-25T00:00:00Z\"}', \
                       NULL)",
            [],
        )
        .expect("seed entry jones");
    }

    #[test]
    fn resolved_view_merges_overlay_over_base_and_falls_back_when_missing() {
        let conn = open_and_migrate();
        seed_two_entries(&conn);
        conn.execute(
            "INSERT INTO reference_entry_overlays (\
               book_slug, entry_key, overlay_json, quality_flags, edited_at, edit_reason\
             ) VALUES ('book_a', 'smith', \
                       '{\"country\":\"United States\",\"verified\":true}', \
                       '[\"verified_by_user\"]', \
                       '2026-06-25T01:00:00Z', 'fix OCR confusion')",
            [],
        )
        .expect("seed overlay");

        // Smith now reads through the overlay: country is overridden,
        // the unchanged base key survives the patch, and the overlay's
        // new key appears in the merged payload. quality_flags is taken
        // wholesale from the overlay.
        let (payload, flags, has_overlay): (String, String, i64) = conn
            .query_row(
                "SELECT payload_json, quality_flags, has_overlay \
                 FROM reference_entries_resolved \
                 WHERE book_slug = 'book_a' AND entry_key = 'smith'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read smith");
        assert!(
            payload.contains("\"country\":\"United States\""),
            "overlay must override base country: {payload}"
        );
        assert!(
            payload.contains("\"year\":1900"),
            "base year must survive the patch: {payload}"
        );
        assert!(
            payload.contains("\"verified\":true"),
            "overlay must contribute new keys: {payload}"
        );
        assert_eq!(flags, "[\"verified_by_user\"]");
        assert_eq!(has_overlay, 1);

        // Jones has no overlay row, so the resolved view returns the
        // base payload unchanged, the base quality_flags (NULL) come
        // through COALESCE, and has_overlay is 0.
        let (payload, flags, has_overlay): (String, Option<String>, i64) = conn
            .query_row(
                "SELECT payload_json, quality_flags, has_overlay \
                 FROM reference_entries_resolved \
                 WHERE book_slug = 'book_a' AND entry_key = 'jones'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read jones");
        assert!(
            payload.contains("\"country\":\"UK\""),
            "base payload must come through unchanged: {payload}"
        );
        assert_eq!(flags, None);
        assert_eq!(has_overlay, 0);
    }

    #[test]
    fn inserting_a_base_row_syncs_the_fts_sidecar_through_the_trigger() {
        let conn = open_and_migrate();
        seed_two_entries(&conn);

        // The trigram tokenizer indexes 3-character shingles, so a
        // straight word match in the seeded fts_text hits the smith
        // row only.
        let mut stmt = conn
            .prepare(
                "SELECT rowid FROM reference_entries_fts \
                 WHERE fts_text MATCH ?1",
            )
            .expect("prepare fts query");
        let rows: Vec<i64> = stmt
            .query_map(["alexander"], |row| row.get(0))
            .expect("query fts")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect fts rows");
        assert_eq!(rows.len(), 1, "exactly one row matches 'alexander'");

        // The base entry the FTS sidecar points to is the seeded smith
        // row, so the rowid maps back to entry_key='smith'.
        let entry_key: String = conn
            .query_row(
                "SELECT entry_key FROM reference_entries \
                 WHERE entry_id = ?1",
                [rows[0]],
                |row| row.get(0),
            )
            .expect("read entry_key for fts rowid");
        assert_eq!(entry_key, "smith");
    }
}
