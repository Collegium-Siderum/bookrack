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
/// chain. Mirrors the catalog flag of the same name in mother doc
/// §5.11.
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
    /// current values (or default 0 on first insert); distill updates
    /// them through dedicated CRUD as entries are upserted.
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
    /// restricts to one. Redirects are not followed; use [`Self::lookup`]
    /// for the disambiguation-shaped reply.
    pub fn lookup_resolved(
        &self,
        book_slug: Option<&str>,
        entry_key: &str,
    ) -> RefsResult<Vec<ResolvedEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.book_slug, r.entry_key, r.headword, r.aliases_json, \
                    r.payload_json, r.source_json, r.quality_flags, \
                    r.edit_reason, r.edited_at, r.has_overlay \
               FROM reference_entries_resolved r \
               JOIN reference_books b USING (book_slug) \
              WHERE r.entry_key = ?1 \
                AND (?2 IS NULL OR r.book_slug = ?2) \
              ORDER BY b.authority_rank DESC, b.built_at ASC",
        )?;
        let rows = stmt
            .query_map(params![entry_key, book_slug], row_to_resolved)?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter().map(parse_resolved).collect()
    }

    /// Disambiguation-shaped lookup. Wraps [`Self::lookup_resolved`]
    /// with one redirect hop and the `primary_by_authority` index.
    ///
    /// Redirect rules (mother doc §5.5):
    /// - If the query yields exactly one hit and that hit's payload
    ///   carries `redirect_to`, the target is looked up under the same
    ///   `book_slug` scope; on success the result reports the target's
    ///   hits with `redirect_followed = Some(original_key)`.
    /// - If following the target would cycle back to the original key,
    ///   the original hit is returned with `redirect_loop` stamped onto
    ///   its `quality_flags`, and `redirect_followed = None`.
    /// - Multi-hit queries and zero-hit queries skip the follow.
    pub fn lookup(&self, book_slug: Option<&str>, entry_key: &str) -> RefsResult<LookupResult> {
        let hits = self.lookup_resolved(book_slug, entry_key)?;

        if hits.len() == 1
            && let Some(target) = redirect_target(&hits[0])
        {
            let target_hits = self.lookup_resolved(book_slug, &target)?;

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
            .lookup_resolved(None, "smith")
            .expect("lookup smith cross-book");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].book_slug, "book_low");
        assert_eq!(hits[1].book_slug, "book_high");

        // Restricting to one book returns just that book's hit.
        let one = refs
            .lookup_resolved(Some("book_high"), "smith")
            .expect("lookup smith in book_high");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].book_slug, "book_high");
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
            .lookup(Some("book_a"), "redirect_source")
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

        let result = refs.lookup(Some("book_a"), "alpha").expect("lookup alpha");
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
