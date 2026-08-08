// SPDX-License-Identifier: Apache-2.0

//! The reference-book read store.
//!
//! `reference.db` holds the distilled entries of every reference book in
//! the library: one shared `reference_entries` base table keyed by
//! `(book_slug, entry_key)`, an `reference_entry_overlays` layer of user
//! edits, an FTS5 trigram sidecar for full-text lookup, and the
//! `reference_entries_resolved` view that returns the patched payload to
//! callers. The schema lives in [`migrate`]; this entry point exposes
//! [`Refs`], the connection handle with CRUD over books, entries, and
//! overlays, [`Refs::lookup_resolved`] for raw view hits, and
//! [`Refs::lookup`] for the disambiguation-shaped reply with redirect
//! follow.

use std::path::Path;

use rusqlite::{Connection, params};
use serde_json::Value as JsonValue;

pub mod indexes;
pub mod migrate;
pub mod types;

pub use migrate::TARGET_VERSION;
pub use types::{IndexKind, IndexSpec, LookupResult, NewBook, NewEntry, NewOverlay, ResolvedEntry};

/// Quality flag stamped on the hits of a self-cancelling redirect
/// chain. Mirrors the flag of the same name in the distill quality-flag
/// catalog (`crates/distill/data/quality_flags.toml`).
pub const REDIRECT_LOOP_FLAG: &str = "redirect_loop";

/// Errors from opening, migrating, or querying `reference.db`.
#[derive(Debug, thiserror::Error)]
pub enum RefsError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A read-only open found a `user_version` past [`TARGET_VERSION`]:
    /// the file was written by a newer schema this build cannot read.
    /// The writable [`Refs::open`] would migrate forward; the read path
    /// refuses rather than touch a database it does not understand.
    #[error(
        "reference.db schema is newer than this build supports: found user_version {found}, supports {supported}"
    )]
    SchemaTooNew { found: i64, supported: i64 },

    /// A read-only open found a `user_version` short of
    /// [`TARGET_VERSION`]: the file exists but the migrations that
    /// build the tables the read path queries have not run. The
    /// writable [`Refs::open`] would migrate it forward; the read path
    /// refuses rather than write to a database a read command opened.
    #[error(
        "reference.db schema is behind this build: found user_version {found}, expects {supported}"
    )]
    SchemaTooOld { found: i64, supported: i64 },

    /// A slug or field path failed identifier validation before being
    /// interpolated into a DDL statement.
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),

    /// The same `IndexSpec::field` appeared twice in the spec list
    /// passed to `register_book` / `indexes::apply`. The previous
    /// implementation silently dropped one of the two; this is now
    /// surfaced explicitly so the book.toml authoring mistake is
    /// visible at registration time.
    #[error("duplicate index field: {0:?}")]
    DuplicateIndex(String),
}

/// The crate's `Result` alias.
pub type RefsResult<T> = Result<T, RefsError>;

/// The reference-store handle.
pub struct Refs {
    conn: Connection,
}

impl Refs {
    /// Open `reference.db` at `path` and bring it to [`TARGET_VERSION`].
    pub fn open(path: &Path) -> RefsResult<Self> {
        let mut conn = bookrack_dbkit::open_production(path)?;
        migrate::migrations().to_latest(&mut conn)?;
        Ok(Self { conn })
    }

    /// Open the `reference.db` at `path` for reading without creating it
    /// and without writing through the connection.
    ///
    /// Mirrors the corpus read-only door in intent: the strict read-only
    /// flags block writes and refuse a missing file rather than
    /// materialize an empty schema, and no migration runs. Both
    /// directions of a `user_version` that is not [`TARGET_VERSION`] are
    /// refused — [`RefsError::SchemaTooNew`] for a file this build
    /// cannot read, [`RefsError::SchemaTooOld`] for one whose tables
    /// have not been built yet — because the alternative is a query
    /// against a schema the caller was not promised. The writable
    /// [`Refs::open`] is the only path that migrates.
    pub fn open_read_only(path: &Path) -> RefsResult<Self> {
        let conn = bookrack_dbkit::open_production_strict_read_only(path)?;
        let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found > TARGET_VERSION {
            return Err(RefsError::SchemaTooNew {
                found,
                supported: TARGET_VERSION,
            });
        }
        if found < TARGET_VERSION {
            return Err(RefsError::SchemaTooOld {
                found,
                supported: TARGET_VERSION,
            });
        }
        Ok(Self { conn })
    }

    /// Open `reference.db` in memory. Convenience for tests and the
    /// `distill verify` dry-run path.
    pub fn open_in_memory() -> RefsResult<Self> {
        let mut conn = Connection::open_in_memory()?;
        migrate::migrations().to_latest(&mut conn)?;
        Ok(Self { conn })
    }

    /// Borrow the underlying `Connection`. Reserved for the diagnose
    /// crate's read-side dump and tests; not part of the stable API.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Register a reference book or update its existing registration in
    /// place. `entry_count` and `parse_warnings` are left at their
    /// current values, which is 0 on first insert and 0 thereafter:
    /// nothing writes those two columns. See [`crate::types::NewBook`]
    /// for what reads the count instead.
    pub fn upsert_book(&self, book: &NewBook) -> RefsResult<()> {
        self.conn.execute(
            "INSERT INTO reference_books (\
               book_slug, schema_name, schema_version, parser_version, \
               title_zh, title_en, edition, publisher, year, isbn, \
               authority_rank, built_at, intake_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
             ON CONFLICT(book_slug) DO UPDATE SET \
               schema_name    = excluded.schema_name, \
               schema_version = excluded.schema_version, \
               parser_version = excluded.parser_version, \
               title_zh       = excluded.title_zh, \
               title_en       = excluded.title_en, \
               edition        = excluded.edition, \
               publisher      = excluded.publisher, \
               year           = excluded.year, \
               isbn           = excluded.isbn, \
               authority_rank = excluded.authority_rank, \
               built_at       = excluded.built_at, \
               intake_id      = excluded.intake_id",
            params![
                book.book_slug,
                book.schema_name,
                book.schema_version,
                book.parser_version,
                book.title_zh,
                book.title_en,
                book.edition,
                book.publisher,
                book.year,
                book.isbn,
                book.authority_rank,
                book.built_at,
                book.intake_id,
            ],
        )?;
        Ok(())
    }

    /// Insert one distilled entry or update the existing row in place,
    /// returning the stable internal `entry_id`. The FTS5 sidecar is
    /// kept in sync by the AI / AU triggers, so callers do not write
    /// to `reference_entries_fts` directly.
    pub fn upsert_entry(&self, entry: &NewEntry) -> RefsResult<i64> {
        let aliases_json = serialize_string_array(&entry.aliases)?;
        let quality_flags = serialize_string_array(&entry.quality_flags)?;
        let payload_json = serde_json::to_string(&entry.payload)?;
        let source_json = serde_json::to_string(&entry.source)?;

        let entry_id: i64 = self.conn.query_row(
            "INSERT INTO reference_entries (\
               book_slug, entry_key, headword, aliases_json, \
               payload_json, fts_text, source_json, quality_flags) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(book_slug, entry_key) DO UPDATE SET \
               headword      = excluded.headword, \
               aliases_json  = excluded.aliases_json, \
               payload_json  = excluded.payload_json, \
               fts_text      = excluded.fts_text, \
               source_json   = excluded.source_json, \
               quality_flags = excluded.quality_flags \
             RETURNING entry_id",
            params![
                entry.book_slug,
                entry.entry_key,
                entry.headword,
                aliases_json,
                payload_json,
                entry.fts_text,
                source_json,
                quality_flags,
            ],
            |row| row.get(0),
        )?;
        Ok(entry_id)
    }

    /// Insert or replace the overlay record for `(book_slug, entry_key)`.
    pub fn upsert_overlay(&self, overlay: &NewOverlay) -> RefsResult<()> {
        let overlay_json = serde_json::to_string(&overlay.overlay)?;
        let quality_flags = match &overlay.quality_flags {
            Some(flags) => Some(serde_json::to_string(flags)?),
            None => None,
        };

        self.conn.execute(
            "INSERT INTO reference_entry_overlays (\
               book_slug, entry_key, overlay_json, quality_flags, \
               base_built_at, edit_reason, edited_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(book_slug, entry_key) DO UPDATE SET \
               overlay_json  = excluded.overlay_json, \
               quality_flags = excluded.quality_flags, \
               base_built_at = excluded.base_built_at, \
               edit_reason   = excluded.edit_reason, \
               edited_at     = excluded.edited_at",
            params![
                overlay.book_slug,
                overlay.entry_key,
                overlay_json,
                quality_flags,
                overlay.base_built_at,
                overlay.edit_reason,
                overlay.edited_at,
            ],
        )?;
        Ok(())
    }

    /// Raw view-side lookup: rows from `reference_entries_resolved`
    /// joined to `reference_books` and ordered by `authority_rank DESC,
    /// built_at ASC`. `book_slug = None` searches every book; `Some`
    /// restricts to one. `exclude` drops the named books from the
    /// result whatever the scope; an empty slice filters nothing.
    /// Redirects are not followed; use [`Self::lookup`] for the
    /// disambiguation-shaped reply.
    pub fn lookup_resolved(
        &self,
        book_slug: Option<&str>,
        entry_key: &str,
        exclude: &[String],
    ) -> RefsResult<Vec<ResolvedEntry>> {
        // The exclusion is variadic, so the tail of the WHERE clause is
        // built to match; the parameters stay bound, never interpolated.
        let exclusion_clause = if exclude.is_empty() {
            String::new()
        } else {
            let placeholders: Vec<String> =
                (0..exclude.len()).map(|i| format!("?{}", i + 3)).collect();
            format!(" AND r.book_slug NOT IN ({})", placeholders.join(", "))
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT r.book_slug, r.entry_key, r.headword, r.aliases_json, \
                    r.payload_json, r.source_json, r.quality_flags, \
                    r.edit_reason, r.edited_at, r.has_overlay \
               FROM reference_entries_resolved r \
               JOIN reference_books b USING (book_slug) \
              WHERE r.entry_key = ?1 \
                AND (?2 IS NULL OR r.book_slug = ?2){exclusion_clause} \
              ORDER BY b.authority_rank DESC, b.built_at ASC",
        ))?;
        let mut bindings: Vec<&dyn rusqlite::ToSql> = vec![&entry_key, &book_slug];
        bindings.extend(exclude.iter().map(|slug| slug as &dyn rusqlite::ToSql));
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bindings), row_to_resolved)?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter().map(parse_resolved).collect()
    }

    /// Disambiguation-shaped lookup. Wraps [`Self::lookup_resolved`]
    /// with one redirect hop, the `primary_by_authority` index, and a
    /// latin-key fallback retry.
    ///
    /// Redirect rules:
    /// - If the query yields exactly one hit and that hit's payload
    ///   carries `redirect_to`, the target is looked up under the same
    ///   `book_slug` scope; on success the result reports the target's
    ///   hits with `redirect_followed = Some(original_key)`.
    /// - If following the target would cycle back to the original key,
    ///   the original hit is returned with `redirect_loop` stamped onto
    ///   its `quality_flags`, and `redirect_followed = None`.
    /// - Multi-hit queries and zero-hit queries skip the follow.
    ///
    /// Fallback rules:
    /// - When the exact query yields no hit, the query is projected
    ///   through [`latin_fallback_key`] and, if the projection differs
    ///   from the original and is non-empty, looked up once more.
    /// - The projection keeps every alphanumeric character — CJK
    ///   included, since those characters count as alphanumeric — so a
    ///   query differing from a stored key only by case, spacing, or
    ///   punctuation is retried once whatever its script. A query that
    ///   already equals its projection (e.g. a compact CJK key) gets
    ///   no second lookup.
    /// - The result's `entry_key` always echoes the original query
    ///   string; canonical `(slug, key)` pairs come from the hits. A
    ///   fallback hit that carries `redirect_to` follows the redirect
    ///   rules above, so `redirect_followed` then names the normalized
    ///   key and the two fields together record the full resolution
    ///   chain.
    ///
    /// `exclude` names books whose entries must not appear. It applies
    /// to every pass — the direct lookup, the fallback retry, and the
    /// redirect target — so an excluded book cannot re-enter the result
    /// by being the destination of a redirect.
    pub fn lookup(
        &self,
        book_slug: Option<&str>,
        entry_key: &str,
        exclude: &[String],
    ) -> RefsResult<LookupResult> {
        let direct = self.lookup_at(book_slug, entry_key, exclude)?;
        if !direct.hits.is_empty() {
            return Ok(direct);
        }
        let fallback = latin_fallback_key(entry_key);
        if fallback == entry_key || fallback.is_empty() {
            return Ok(direct);
        }
        let mut retried = self.lookup_at(book_slug, &fallback, exclude)?;
        retried.entry_key = entry_key.to_string();
        Ok(retried)
    }

    /// One lookup pass at a literal key: resolved-view hits plus the
    /// redirect hop, without the latin-key fallback retry.
    fn lookup_at(
        &self,
        book_slug: Option<&str>,
        entry_key: &str,
        exclude: &[String],
    ) -> RefsResult<LookupResult> {
        let hits = self.lookup_resolved(book_slug, entry_key, exclude)?;

        if hits.len() == 1
            && let Some(target) = redirect_target(&hits[0])
        {
            let target_hits = self.lookup_resolved(book_slug, &target, exclude)?;

            let loops_back = target_hits
                .iter()
                .any(|h| redirect_target(h).as_deref() == Some(entry_key));

            if loops_back {
                let looped: Vec<ResolvedEntry> = hits
                    .into_iter()
                    .map(|mut hit| {
                        if !hit.quality_flags.iter().any(|f| f == REDIRECT_LOOP_FLAG) {
                            hit.quality_flags.push(REDIRECT_LOOP_FLAG.to_string());
                        }
                        hit
                    })
                    .collect();
                return Ok(LookupResult {
                    entry_key: entry_key.to_string(),
                    primary_by_authority: Some(0),
                    hits: looped,
                    redirect_followed: None,
                });
            }

            if !target_hits.is_empty() {
                return Ok(LookupResult {
                    entry_key: entry_key.to_string(),
                    primary_by_authority: Some(0),
                    hits: target_hits,
                    redirect_followed: Some(entry_key.to_string()),
                });
            }
        }

        let primary = (!hits.is_empty()).then_some(0);
        Ok(LookupResult {
            entry_key: entry_key.to_string(),
            hits,
            primary_by_authority: primary,
            redirect_followed: None,
        })
    }

    /// Attach the per-book physical lookup paths declared in
    /// `book.toml`'s `[[indexes]]` to `reference_entries`. See the
    /// [`indexes`] module for the column / index name scheme.
    pub fn register_book(&mut self, book_slug: &str, specs: &[IndexSpec]) -> RefsResult<()> {
        indexes::apply(&self.conn, book_slug, specs)
    }
}

/// Project a query string into the latin lookup-key form: lower-case
/// with every non-alphanumeric character removed. Mirrors the distill
/// side's `KeyNormalizer::NormalizeLatinKey`; a cli-side test pins the
/// two together.
pub fn latin_fallback_key(query: &str) -> String {
    query
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Serialize a string array to its FTS sidecar / overlay JSON form, or
/// to `NULL` when empty. The base schema's `aliases_json` and
/// `quality_flags` columns are nullable specifically so an entry with
/// no aliases or no flags carries no JSON noise.
fn serialize_string_array(items: &[String]) -> RefsResult<Option<String>> {
    if items.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(items)?))
    }
}

/// Pull the redirect target out of an entry payload, if any. Only the
/// string form is honoured: a non-string `redirect_to` is a malformed
/// distill artifact and falls through to the no-redirect branch.
fn redirect_target(entry: &ResolvedEntry) -> Option<String> {
    entry
        .payload
        .as_object()?
        .get("redirect_to")?
        .as_str()
        .map(str::to_string)
}

/// One row of the resolved view as raw strings, before JSON decoding.
struct RawResolved {
    book_slug: String,
    entry_key: String,
    headword: String,
    aliases_json: Option<String>,
    payload_json: String,
    source_json: String,
    quality_flags: Option<String>,
    edit_reason: Option<String>,
    edited_at: Option<String>,
    has_overlay: i64,
}

fn row_to_resolved(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawResolved> {
    Ok(RawResolved {
        book_slug: row.get(0)?,
        entry_key: row.get(1)?,
        headword: row.get(2)?,
        aliases_json: row.get(3)?,
        payload_json: row.get(4)?,
        source_json: row.get(5)?,
        quality_flags: row.get(6)?,
        edit_reason: row.get(7)?,
        edited_at: row.get(8)?,
        has_overlay: row.get(9)?,
    })
}

fn parse_resolved(raw: RawResolved) -> RefsResult<ResolvedEntry> {
    let aliases = match raw.aliases_json {
        Some(s) => serde_json::from_str(&s)?,
        None => Vec::new(),
    };
    let quality_flags = match raw.quality_flags {
        Some(s) => serde_json::from_str(&s)?,
        None => Vec::new(),
    };
    let payload: JsonValue = serde_json::from_str(&raw.payload_json)?;
    let source: JsonValue = serde_json::from_str(&raw.source_json)?;

    Ok(ResolvedEntry {
        book_slug: raw.book_slug,
        entry_key: raw.entry_key,
        headword: raw.headword,
        aliases,
        payload,
        source,
        quality_flags,
        edit_reason: raw.edit_reason,
        edited_at: raw.edited_at,
        has_overlay: raw.has_overlay != 0,
    })
}

#[cfg(test)]
mod refs_tests {
    use super::*;
    use serde_json::json;

    fn fresh_refs() -> Refs {
        Refs::open_in_memory().expect("open in-memory refs")
    }

    fn sample_book(slug: &str, authority_rank: i64, built_at: &str) -> NewBook {
        NewBook {
            book_slug: slug.to_string(),
            schema_name: "name_translation".to_string(),
            schema_version: 1,
            parser_version: "0.1.0".to_string(),
            title_zh: format!("Book {slug}"),
            title_en: None,
            edition: None,
            publisher: None,
            year: None,
            isbn: None,
            authority_rank,
            built_at: built_at.to_string(),
            intake_id: None,
        }
    }

    fn sample_entry(slug: &str, entry_key: &str, headword: &str, payload: JsonValue) -> NewEntry {
        NewEntry {
            book_slug: slug.to_string(),
            entry_key: entry_key.to_string(),
            headword: headword.to_string(),
            aliases: vec![],
            payload,
            fts_text: headword.to_string(),
            source: json!({
                "book_slug": slug,
                "page": 1,
                "sheet": 1,
                "distill_run_id": "2026-06-25T00:00:00Z",
            }),
            quality_flags: vec![],
        }
    }

    /// Reflect column names through `Statement::column_names()` on a
    /// zero-row `SELECT *`. The `pragma_table_info(...)` form is
    /// served from rusqlite's compiled-schema cache and would miss
    /// columns added by an earlier ALTER on the same connection.
    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        let stmt = conn
            .prepare(&format!("SELECT * FROM {table} LIMIT 0"))
            .expect("prepare SELECT * LIMIT 0");
        stmt.column_names().contains(&column)
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                params![name],
                |row| row.get(0),
            )
            .expect("query sqlite_master");
        count > 0
    }

    /// `entry_count` and `parse_warnings` stay at 0 no matter what is
    /// written through this crate's public surface.
    ///
    /// Both column docs describe them as counters `Refs` maintains as
    /// entries are upserted, which no code path does. The test holds
    /// the docs to what the code actually guarantees, so a future
    /// writer either updates them or fails here.
    #[test]
    fn the_entry_counters_on_reference_books_are_never_written() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("fake_book", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book");
        for key in ["a", "b", "c"] {
            refs.upsert_entry(&sample_entry("fake_book", key, key, json!({})))
                .expect("upsert entry");
        }
        // A second registration of the same book, the other write that
        // touches this row.
        refs.upsert_book(&sample_book("fake_book", 20, "2026-06-26T00:00:00Z"))
            .expect("re-upsert book");

        let (entry_count, parse_warnings): (i64, i64) = refs
            .connection()
            .query_row(
                "SELECT entry_count, parse_warnings FROM reference_books \
                 WHERE book_slug = 'fake_book'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read the counter columns");
        assert_eq!(entry_count, 0, "three entries did not move entry_count");
        assert_eq!(parse_warnings, 0);

        let live: i64 = refs
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM reference_entries WHERE book_slug = 'fake_book'",
                [],
                |row| row.get(0),
            )
            .expect("count entries");
        assert_eq!(live, 3, "the rows a reader has to count instead are there");
    }

    #[test]
    fn register_book_creates_generated_columns_and_partial_indexes() {
        let mut refs = fresh_refs();
        refs.upsert_book(&sample_book("fake_book", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book");

        refs.register_book(
            "fake_book",
            &[
                IndexSpec {
                    field: "country".to_string(),
                    kind: IndexKind::Btree,
                },
                IndexSpec {
                    field: "year_span.birth".to_string(),
                    kind: IndexKind::Btree,
                },
            ],
        )
        .expect("register fake_book");

        let conn = refs.connection();
        assert!(
            column_exists(conn, "reference_entries", "gencol__fake_ubook__country"),
            "gencol__fake_ubook__country must exist"
        );
        assert!(
            column_exists(
                conn,
                "reference_entries",
                "gencol__fake_ubook__year_uspan_dbirth"
            ),
            "gencol__fake_ubook__year_uspan_dbirth must exist (. -> _d, _ -> _u)"
        );
        assert!(index_exists(conn, "ix_ref__fake_ubook__country"));
        assert!(index_exists(conn, "ix_ref__fake_ubook__year_uspan_dbirth"));

        // The partial WHERE clause persists into sqlite_master.
        let idx_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master \
                 WHERE type = 'index' AND name = 'ix_ref__fake_ubook__country'",
                [],
                |row| row.get(0),
            )
            .expect("read index ddl");
        assert!(
            idx_sql.contains("WHERE book_slug = 'fake_book'"),
            "partial index must persist the WHERE clause: {idx_sql}"
        );

        // The generated column actually exposes the json_extract path.
        refs.upsert_entry(&sample_entry(
            "fake_book",
            "smith",
            "Smith",
            json!({"country": "USA", "year_span": {"birth": 1900}}),
        ))
        .expect("upsert entry");
        let country: String = conn
            .query_row(
                "SELECT gencol__fake_ubook__country FROM reference_entries \
                 WHERE book_slug = 'fake_book' AND entry_key = 'smith'",
                [],
                |row| row.get(0),
            )
            .expect("read gencol_country");
        assert_eq!(country, "USA");
        let birth: i64 = conn
            .query_row(
                "SELECT gencol__fake_ubook__year_uspan_dbirth FROM reference_entries \
                 WHERE book_slug = 'fake_book' AND entry_key = 'smith'",
                [],
                |row| row.get(0),
            )
            .expect("read gencol_year_span_birth");
        assert_eq!(birth, 1900);
    }

    #[test]
    fn register_book_is_idempotent() {
        let mut refs = fresh_refs();
        refs.upsert_book(&sample_book("fake_book", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book");
        let specs = vec![IndexSpec {
            field: "country".to_string(),
            kind: IndexKind::Btree,
        }];
        refs.register_book("fake_book", &specs)
            .expect("first register");
        refs.register_book("fake_book", &specs)
            .expect("second register must be a no-op");
    }

    #[test]
    fn register_book_keeps_each_book_isolated() {
        let mut refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_book(&sample_book("book_b", 5, "2026-06-25T00:01:00Z"))
            .expect("upsert book_b");

        let specs_a = vec![
            IndexSpec {
                field: "country".to_string(),
                kind: IndexKind::Btree,
            },
            IndexSpec {
                field: "year_span.birth".to_string(),
                kind: IndexKind::Btree,
            },
        ];
        let specs_b = vec![
            IndexSpec {
                field: "gender".to_string(),
                kind: IndexKind::Btree,
            },
            IndexSpec {
                field: "variants".to_string(),
                kind: IndexKind::Btree,
            },
        ];

        refs.register_book("book_a", &specs_a).expect("register a");
        refs.register_book("book_b", &specs_b).expect("register b");

        let conn = refs.connection();
        for col in [
            "gencol__book_ua__country",
            "gencol__book_ua__year_uspan_dbirth",
            "gencol__book_ub__gender",
            "gencol__book_ub__variants",
        ] {
            assert!(
                column_exists(conn, "reference_entries", col),
                "expected column {col} after both registrations"
            );
        }
        for ix in [
            "ix_ref__book_ua__country",
            "ix_ref__book_ua__year_uspan_dbirth",
            "ix_ref__book_ub__gender",
            "ix_ref__book_ub__variants",
        ] {
            assert!(
                index_exists(conn, ix),
                "expected index {ix} after both registrations"
            );
        }
    }

    #[test]
    fn invalid_slug_is_rejected_before_any_ddl() {
        let mut refs = fresh_refs();
        let err = refs
            .register_book(
                "book; DROP TABLE reference_entries; --",
                &[IndexSpec {
                    field: "country".to_string(),
                    kind: IndexKind::Btree,
                }],
            )
            .unwrap_err();
        assert!(
            matches!(err, RefsError::InvalidIdentifier(_)),
            "expected InvalidIdentifier, got {err:?}"
        );
    }

    #[test]
    fn lookup_resolved_orders_cross_book_hits_by_authority_rank_then_built_at() {
        let refs = fresh_refs();
        // book_low has higher authority_rank; book_high has lower
        // authority_rank but an earlier built_at, so the tiebreak comes
        // through only on the rank dimension.
        refs.upsert_book(&sample_book("book_low", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_low");
        refs.upsert_book(&sample_book("book_high", 1, "2026-06-24T00:00:00Z"))
            .expect("upsert book_high");

        refs.upsert_entry(&sample_entry(
            "book_low",
            "smith",
            "Smith (low)",
            json!({"country": "USA"}),
        ))
        .expect("upsert smith in book_low");
        refs.upsert_entry(&sample_entry(
            "book_high",
            "smith",
            "Smith (high)",
            json!({"country": "UK"}),
        ))
        .expect("upsert smith in book_high");

        let hits = refs
            .lookup_resolved(None, "smith", &[])
            .expect("lookup smith cross-book");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].book_slug, "book_low");
        assert_eq!(hits[1].book_slug, "book_high");

        // Restricting to one book returns just that book's hit.
        let one = refs
            .lookup_resolved(Some("book_high"), "smith", &[])
            .expect("lookup smith in book_high");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].book_slug, "book_high");
    }

    #[test]
    fn lookup_resolved_drops_the_excluded_books_and_keeps_the_rest_in_order() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_low", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_low");
        refs.upsert_book(&sample_book("book_mid", 5, "2026-06-24T12:00:00Z"))
            .expect("upsert book_mid");
        refs.upsert_book(&sample_book("book_high", 1, "2026-06-24T00:00:00Z"))
            .expect("upsert book_high");
        for slug in ["book_low", "book_mid", "book_high"] {
            refs.upsert_entry(&sample_entry(
                slug,
                "smith",
                "Smith",
                json!({"country": "USA"}),
            ))
            .expect("upsert smith");
        }

        // Two exclusions at once, so the variadic tail of the WHERE
        // clause is exercised rather than a single-placeholder case.
        let hits = refs
            .lookup_resolved(
                None,
                "smith",
                &["book_low".to_string(), "book_high".to_string()],
            )
            .expect("lookup smith excluding two books");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].book_slug, "book_mid");

        // Excluding just the top-ranked book leaves the remaining two in
        // their authority order.
        let hits = refs
            .lookup_resolved(None, "smith", &["book_low".to_string()])
            .expect("lookup smith excluding the top book");
        let slugs: Vec<&str> = hits.iter().map(|h| h.book_slug.as_str()).collect();
        assert_eq!(slugs, ["book_mid", "book_high"]);

        // A slug that names no registered book excludes nothing.
        let hits = refs
            .lookup_resolved(None, "smith", &["no_such_book".to_string()])
            .expect("lookup smith excluding an unknown book");
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn an_excluded_book_does_not_re_enter_as_a_redirect_target() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_book(&sample_book("book_b", 5, "2026-06-25T00:01:00Z"))
            .expect("upsert book_b");
        // The only hit for the key redirects, and the target lives in
        // the book being excluded.
        refs.upsert_entry(&sample_entry(
            "book_a",
            "redirect_source",
            "Redirect Source",
            json!({"redirect_to": "target"}),
        ))
        .expect("upsert redirect_source");
        refs.upsert_entry(&sample_entry(
            "book_b",
            "target",
            "Target",
            json!({"country": "USA"}),
        ))
        .expect("upsert target in book_b");

        // Without the exclusion the redirect resolves into book_b.
        let followed = refs
            .lookup(None, "redirect_source", &[])
            .expect("lookup redirect_source");
        assert_eq!(
            followed.redirect_followed.as_deref(),
            Some("redirect_source")
        );
        assert_eq!(followed.hits[0].book_slug, "book_b");

        // With book_b excluded the target is unreachable, so the
        // redirect is not reported as followed and book_b contributes
        // nothing to the hits.
        let result = refs
            .lookup(None, "redirect_source", &["book_b".to_string()])
            .expect("lookup redirect_source excluding book_b");
        assert_eq!(result.redirect_followed, None);
        assert!(
            result.hits.iter().all(|h| h.book_slug != "book_b"),
            "the excluded book must not re-enter through the redirect: {:?}",
            result.hits.iter().map(|h| &h.book_slug).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_latin_fallback_retry_carries_the_exclusion() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_book(&sample_book("book_b", 5, "2026-06-25T00:01:00Z"))
            .expect("upsert book_b");
        // Both books store the compact key; the query differs from it
        // only by spacing, so only the fallback pass can find them.
        for slug in ["book_a", "book_b"] {
            refs.upsert_entry(&sample_entry(
                slug,
                "objeta",
                "Objet a",
                json!({"country": "FR"}),
            ))
            .expect("upsert objeta");
        }

        let result = refs
            .lookup(None, "Objet a", &["book_a".to_string()])
            .expect("fallback lookup excluding book_a");
        assert_eq!(
            result.entry_key, "Objet a",
            "the original query echoes back"
        );
        let slugs: Vec<&str> = result.hits.iter().map(|h| h.book_slug.as_str()).collect();
        assert_eq!(
            slugs,
            ["book_b"],
            "the exclusion must survive into the fallback retry",
        );
    }

    #[test]
    fn lookup_follows_a_redirect_one_hop_and_reports_the_original_key() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "redirect_source",
            "Redirect Source",
            json!({"redirect_to": "target"}),
        ))
        .expect("upsert redirect_source");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "target",
            "Target",
            json!({"country": "USA"}),
        ))
        .expect("upsert target");

        let result = refs
            .lookup(Some("book_a"), "redirect_source", &[])
            .expect("lookup redirect_source");
        assert_eq!(result.entry_key, "redirect_source");
        assert_eq!(result.redirect_followed.as_deref(), Some("redirect_source"));
        assert_eq!(result.primary_by_authority, Some(0));
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].entry_key, "target");
        assert_eq!(result.hits[0].payload, json!({"country": "USA"}));
    }

    #[test]
    fn lookup_detects_a_two_node_redirect_loop_and_stamps_the_flag() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "alpha",
            "Alpha",
            json!({"redirect_to": "beta"}),
        ))
        .expect("upsert alpha");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "beta",
            "Beta",
            json!({"redirect_to": "alpha"}),
        ))
        .expect("upsert beta");

        let result = refs
            .lookup(Some("book_a"), "alpha", &[])
            .expect("lookup alpha");
        assert_eq!(result.entry_key, "alpha");
        assert_eq!(result.redirect_followed, None);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].entry_key, "alpha");
        assert!(
            result.hits[0]
                .quality_flags
                .iter()
                .any(|f| f == REDIRECT_LOOP_FLAG),
            "expected redirect_loop flag, got {:?}",
            result.hits[0].quality_flags
        );
    }

    #[test]
    fn lookup_exact_hit_skips_the_latin_fallback() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "objeta",
            "Objet a",
            json!({"lang": "fr"}),
        ))
        .expect("upsert objeta");

        let result = refs.lookup(Some("book_a"), "objeta", &[]).expect("lookup");
        assert_eq!(result.entry_key, "objeta");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].entry_key, "objeta");
        assert_eq!(result.redirect_followed, None);
    }

    #[test]
    fn lookup_falls_back_to_the_latin_key_and_echoes_the_original_query() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "objeta",
            "Objet a",
            json!({"lang": "fr"}),
        ))
        .expect("upsert objeta");

        let result = refs.lookup(Some("book_a"), "Objet a", &[]).expect("lookup");
        assert_eq!(result.entry_key, "Objet a");
        assert_eq!(result.primary_by_authority, Some(0));
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].entry_key, "objeta");
    }

    #[test]
    fn lookup_fallback_miss_returns_empty_hits_with_the_original_query() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");

        let result = refs
            .lookup(Some("book_a"), "No Such Key", &[])
            .expect("lookup");
        assert_eq!(result.entry_key, "No Such Key");
        assert!(result.hits.is_empty());
        assert_eq!(result.primary_by_authority, None);
        assert_eq!(result.redirect_followed, None);
    }

    #[test]
    fn lookup_fallback_hit_still_follows_a_redirect() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "objeta",
            "Objet a",
            json!({"redirect_to": "target"}),
        ))
        .expect("upsert objeta");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "target",
            "Target",
            json!({"lang": "fr"}),
        ))
        .expect("upsert target");

        let result = refs.lookup(Some("book_a"), "Objet a", &[]).expect("lookup");
        assert_eq!(result.entry_key, "Objet a");
        assert_eq!(result.redirect_followed.as_deref(), Some("objeta"));
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].entry_key, "target");
    }

    #[test]
    fn lookup_with_multiple_hits_skips_the_redirect_follow() {
        // Two books resolve the same key, and both entries carry a
        // redirect. Following either would silently pick a side, so
        // the follow is gated on exactly one hit: the caller gets the
        // unresolved pair back for disambiguation.
        let refs = fresh_refs();
        for slug in ["book_a", "book_b"] {
            refs.upsert_book(&sample_book(slug, 10, "2026-06-25T00:00:00Z"))
                .expect("upsert book");
            refs.upsert_entry(&sample_entry(
                slug,
                "shared",
                "Shared",
                json!({"redirect_to": "target"}),
            ))
            .expect("upsert shared entry");
            refs.upsert_entry(&sample_entry(
                slug,
                "target",
                "Target",
                json!({"country": "USA"}),
            ))
            .expect("upsert target entry");
        }

        let result = refs.lookup(None, "shared", &[]).expect("global lookup");
        assert_eq!(result.hits.len(), 2, "both books answer");
        assert_eq!(result.redirect_followed, None, "no follow on multi-hit");
        assert!(
            result.hits.iter().all(|h| h.entry_key == "shared"),
            "the unresolved originals come back, not the targets: {:?}",
            result
                .hits
                .iter()
                .map(|h| h.entry_key.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn latin_fallback_key_keeps_cjk_characters() {
        // `char::is_alphanumeric` is true for CJK, so the projection
        // lowercases and strips punctuation but keeps the characters:
        // a spaced CJK query projects onto its compact form, while an
        // already-compact CJK key projects to itself.
        assert_eq!(latin_fallback_key("Foo-Bar"), "foobar");
        assert_eq!(latin_fallback_key("\u{6C49} \u{5B57}"), "\u{6C49}\u{5B57}");
        assert_eq!(latin_fallback_key("\u{6C49}\u{5B57}"), "\u{6C49}\u{5B57}");
    }

    #[test]
    fn lookup_retries_a_spaced_cjk_query_onto_the_compact_key() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        refs.upsert_entry(&sample_entry(
            "book_a",
            "\u{6C49}\u{5B57}",
            "Han Zi",
            json!({"lang": "zh"}),
        ))
        .expect("upsert compact CJK key");

        // The spaced form misses exactly, projects onto the compact
        // key, and the retry hits — same treatment as a latin key.
        let result = refs
            .lookup(Some("book_a"), "\u{6C49} \u{5B57}", &[])
            .expect("lookup spaced CJK");
        assert_eq!(result.entry_key, "\u{6C49} \u{5B57}");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].entry_key, "\u{6C49}\u{5B57}");
    }

    #[test]
    fn lookup_skips_the_fallback_when_the_projection_is_empty_or_unchanged() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");

        for query in ["", "  ", "!!!"] {
            let result = refs.lookup(Some("book_a"), query, &[]).expect("lookup");
            assert_eq!(result.entry_key, query);
            assert!(result.hits.is_empty(), "query {query:?} must stay empty");
        }
    }

    #[test]
    fn upsert_entry_returns_the_same_entry_id_on_conflict() {
        let refs = fresh_refs();
        refs.upsert_book(&sample_book("book_a", 10, "2026-06-25T00:00:00Z"))
            .expect("upsert book_a");
        let first = refs
            .upsert_entry(&sample_entry(
                "book_a",
                "smith",
                "Smith",
                json!({"country": "USA"}),
            ))
            .expect("insert smith");
        let second = refs
            .upsert_entry(&sample_entry(
                "book_a",
                "smith",
                "Smith (updated)",
                json!({"country": "United States"}),
            ))
            .expect("update smith");
        assert_eq!(first, second, "entry_id is stable across upsert");

        // The conflict path also keeps the FTS sidecar in sync: the
        // updated headword is what the trigger AU pair re-indexed.
        let conn = refs.connection();
        let matched: i64 = conn
            .query_row(
                "SELECT count(*) FROM reference_entries_fts \
                 WHERE headword MATCH 'Smith'",
                [],
                |row| row.get(0),
            )
            .expect("count fts hits");
        assert_eq!(matched, 1);
    }
}

#[cfg(test)]
mod read_only_tests {
    use super::*;
    use tempfile::tempdir;

    /// The read-only door refuses a path with no `reference.db` rather
    /// than materializing an empty schema through it.
    #[test]
    fn open_read_only_refuses_missing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("reference.db");

        let Err(err) = Refs::open_read_only(&path) else {
            panic!("missing file must fail to open");
        };
        assert!(
            matches!(err, RefsError::Sqlite(_)),
            "expected a SQLite open failure, got {err}"
        );
        assert!(
            !path.exists(),
            "read-only open must not create reference.db"
        );
    }

    /// A file already built by the writable door reads back through the
    /// read-only door, and the connection rejects writes.
    #[test]
    fn open_read_only_reads_existing_and_blocks_writes() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("reference.db");
        drop(Refs::open(&path).expect("build reference.db"));

        let refs = Refs::open_read_only(&path).expect("open existing read-only");
        let count: i64 = refs
            .connection()
            .query_row("SELECT count(*) FROM reference_books", [], |row| row.get(0))
            .expect("read reference_books");
        assert_eq!(count, 0);

        let err = refs
            .connection()
            .execute("CREATE TABLE t (x INTEGER)", [])
            .expect_err("read-only connection must reject writes");
        assert!(
            format!("{err}").to_lowercase().contains("readonly"),
            "expected a readonly rejection, got {err}"
        );
    }

    /// A `user_version` short of the target is refused: the file
    /// exists but holds none of the tables the read path queries, and
    /// migrating it is the writable door's job.
    #[test]
    fn open_read_only_refuses_older_schema() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("reference.db");
        // A half-created database: present, empty, `user_version` 0.
        std::fs::File::create(&path).expect("create an unmigrated reference.db");

        let Err(err) = Refs::open_read_only(&path) else {
            panic!("a schema behind the target must be refused");
        };
        assert!(
            matches!(
                err,
                RefsError::SchemaTooOld {
                    found,
                    supported,
                } if found == 0 && supported == TARGET_VERSION
            ),
            "expected SchemaTooOld, got {err}"
        );
    }

    /// A `user_version` past the target is refused: the file was written
    /// by a newer schema this build cannot read.
    #[test]
    fn open_read_only_refuses_newer_schema() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("reference.db");
        {
            let refs = Refs::open(&path).expect("build reference.db");
            refs.connection()
                .pragma_update(None, "user_version", TARGET_VERSION + 1)
                .expect("bump user_version");
        }

        let Err(err) = Refs::open_read_only(&path) else {
            panic!("newer schema must be refused");
        };
        assert!(
            matches!(
                err,
                RefsError::SchemaTooNew {
                    found,
                    supported,
                } if found == TARGET_VERSION + 1 && supported == TARGET_VERSION
            ),
            "expected SchemaTooNew, got {err}"
        );
    }
}
