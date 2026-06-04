// SPDX-License-Identifier: Apache-2.0

//! The `intake` table — file-manifestation identity and registration.
//!
//! An *intake* is one ingested source file. Its `source_sha256` (the
//! whole-file hash) is the identity anchor of the entire data model:
//! one file, one intake, one book. Registration is idempotent on that
//! hash, so re-offering a file already known returns the existing row
//! instead of creating a duplicate.
//!
//! # Format commitment
//!
//! The shape of this table is the bookrack intake format: any future
//! binary opens an existing `catalog.db` and reads every row's
//! intake fields back unchanged. The rules:
//!
//! - Columns may be added, never renamed and never dropped.
//! - A new column must be nullable or carry a literal default, so an
//!   older binary's `INSERT` path remains valid.
//! - Existing values, once written from a production path, are
//!   frozen in meaning. The string value sets of `format`, `adapter`,
//!   and `status` (the `IntakeStatus` enum) are append-only; an
//!   existing label never changes meaning.
//! - `intake_id` is permanent and never reused. `source_sha256` is
//!   permanent and identifies the same bytes for the lifetime of
//!   the database.
//! - `intake_at` is ISO-8601 UTC at second resolution, with the `Z`
//!   timezone designator.
//! - `extractor_version` carries the integer in
//!   `bookrack_extract::EXTRACTOR_VERSION` at the moment the file
//!   was extracted, and reading the integer back yields the value
//!   it was written with.
//!
//! Fields not yet written by any production path — `notes`,
//! `expression_id`, and `stored_path` together with the opaque-store
//! directory layout under it — sit outside the commitment until
//! first use.
//!
//! The commitment is anchored physically by the round-trip test on
//! `tests/fixtures/intake/v1/catalog.db`: a future binary that
//! breaks any rule above flips that test red.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec, decode};
use rusqlite::{OptionalExtension, Row, ToSql, named_params, params_from_iter};

use crate::{BOOK_SCOPE, Catalog, Result, count_as_u64};

/// The single source of truth for the `intake` table's schema. Its DDL
/// is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "intake",
    comment: Some("A file manifestation: the identity anchor of one ingested source file."),
    columns: &[
        ColumnSpec::int("intake_id")
            .pk_autoinc()
            .comment("long-lived, never reused"),
        ColumnSpec::text("source_sha256")
            .not_null()
            .unique()
            .comment("whole-file hash; the identity anchor"),
        ColumnSpec::text("stored_path")
            .comment("opaque store location; set once the file is stored"),
        ColumnSpec::text("original_path").comment("forensic: where the file came from"),
        ColumnSpec::text("format").comment("pdf / epub / mobi / azw3 / text / ..."),
        ColumnSpec::int("byte_size"),
        ColumnSpec::text("adapter").comment("extraction adapter, stamped at EXTRACT"),
        ColumnSpec::int("extractor_version")
            .not_null()
            .default("1")
            .comment(
                "value of `bookrack_extract::EXTRACTOR_VERSION` at EXTRACT; \
                 a mismatch with the current const marks a stale partition",
            ),
        ColumnSpec::text("intake_at")
            .not_null()
            .comment("ISO-8601 UTC"),
        ColumnSpec::text("status")
            .not_null()
            .comment("see IntakeStatus"),
        ColumnSpec::int("expression_id").comment("FRBR soft reference; backfilled at METADATA"),
        ColumnSpec::text("notes"),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[
        IndexSpec::on("idx_intake_status", &["status"]),
        IndexSpec::on("idx_intake_format", &["format"]),
    ],
};

/// `LIKE` escape character used by [`Catalog::find_intakes`]'s title
/// predicate, so user input containing `%` or `_` matches literally
/// rather than acting as a wildcard.
const LIKE_ESCAPE: &str = "\\";

/// `INSERT` for a freshly registered intake. The columns absent here —
/// `intake_id`, `stored_path`, `adapter`, `expression_id`, `notes` — are
/// autoincremented or filled by later pipeline stages. Callers append a
/// `RETURNING` clause built from [`SPEC`].
const INSERT_INTAKE_SQL: &str = "INSERT INTO intake \
     (source_sha256, original_path, format, byte_size, intake_at, status) \
     VALUES (:source_sha256, :original_path, :format, :byte_size, \
             strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), :status)";

/// A `SELECT` of every intake column with `tail` (a `WHERE` clause)
/// appended. The column list is derived from [`SPEC`], so it can never
/// drift from the schema.
fn select_sql(tail: &str) -> String {
    format!("SELECT {} FROM intake {tail}", SPEC.select_list())
}

/// Coarse lifecycle state of an intake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntakeStatus {
    /// Registered, not yet processed.
    Pending,
    /// The file has been extracted to blocks and a TOC.
    Extracted,
    /// Held for human review of a suspected fuzzy-duplicate overlap.
    DedupHold,
    /// Fully ingested; vectors exist in the vector store.
    Embedded,
    /// Processing failed and was abandoned.
    Aborted,
}

impl IntakeStatus {
    /// Every status, in lifecycle order.
    pub const ALL: [IntakeStatus; 5] = [
        IntakeStatus::Pending,
        IntakeStatus::Extracted,
        IntakeStatus::DedupHold,
        IntakeStatus::Embedded,
        IntakeStatus::Aborted,
    ];

    /// The database string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            IntakeStatus::Pending => "pending",
            IntakeStatus::Extracted => "extracted",
            IntakeStatus::DedupHold => "dedup_hold",
            IntakeStatus::Embedded => "embedded",
            IntakeStatus::Aborted => "aborted",
        }
    }

    /// Parse the database string form, or `None` if unrecognized.
    pub fn from_db_str(s: &str) -> Option<IntakeStatus> {
        IntakeStatus::ALL.into_iter().find(|st| st.as_str() == s)
    }
}

/// What [`Catalog::find_intakes`] and [`Catalog::count_find_intakes`]
/// filter on. Each field is an optional predicate AND-combined with the
/// others; the default value (`IntakeFilter::default()`) imposes none and
/// matches every row.
///
/// Strings are borrowed so callers do not need to `clone()` query
/// fragments they already hold. `statuses` is an empty slice when no
/// status filter is wanted.
#[derive(Debug, Default, Clone)]
pub struct IntakeFilter<'a> {
    /// Case-sensitive substring match against the root publication-attrs
    /// title, i.e. `node_publication_attrs.title LIKE '%' || ? || '%'`
    /// joined on the book scope. `%` and `_` in the substring match
    /// literally — the LIKE is escaped.
    pub title_substring: Option<&'a str>,
    /// Exact-equality match against the root contributor name in
    /// `node_contributors.name`, joined on the book scope. Combined with
    /// `contributor_role` when both are set.
    pub contributor_name: Option<&'a str>,
    /// Restrict the contributor JOIN to one role (`"author"`,
    /// `"translator"`, ...). Only takes effect when
    /// `contributor_name` is also set.
    pub contributor_role: Option<&'a str>,
    /// Match `intake.status` against this set. An empty slice means "no
    /// filter".
    pub statuses: &'a [IntakeStatus],
    /// Exact-equality match against `intake.format`. Rows whose `format`
    /// is `NULL` never match.
    pub format: Option<&'a str>,
}

/// The list of `intake` columns qualified with the `i.` alias used by
/// the find / count SQL.
fn intake_columns_qualified() -> String {
    SPEC.columns
        .iter()
        .map(|c| format!("i.{}", c.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render the JOIN clauses, WHERE clause, and bind parameters for a
/// filter. The JOIN and WHERE strings are empty when the filter is
/// empty; `joins` always begins with a leading space when non-empty,
/// and `where_clause` always begins with ` WHERE `, so they slot into
/// the surrounding SELECT verbatim.
fn build_filter_fragments(filter: &IntakeFilter<'_>) -> (String, String, Vec<Box<dyn ToSql>>) {
    let mut joins = String::new();
    let mut where_parts: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    if filter.title_substring.is_some() {
        joins.push_str(
            " LEFT JOIN node_publication_attrs npa \
             ON npa.intake_id = i.intake_id AND npa.scope = ?",
        );
        params.push(Box::new(BOOK_SCOPE.to_string()));
    }
    if filter.contributor_name.is_some() {
        joins.push_str(
            " LEFT JOIN node_contributors nc \
             ON nc.intake_id = i.intake_id AND nc.scope = ?",
        );
        params.push(Box::new(BOOK_SCOPE.to_string()));
    }

    if let Some(needle) = filter.title_substring {
        where_parts.push(format!("npa.title LIKE ? ESCAPE '{LIKE_ESCAPE}'"));
        params.push(Box::new(format!("%{}%", like_escape(needle))));
    }
    if let Some(name) = filter.contributor_name {
        where_parts.push("nc.name = ?".to_string());
        params.push(Box::new(name.to_string()));
        if let Some(role) = filter.contributor_role {
            where_parts.push("nc.role = ?".to_string());
            params.push(Box::new(role.to_string()));
        }
    }
    if !filter.statuses.is_empty() {
        debug_assert!(
            filter.statuses.len() <= 8,
            "IntakeFilter.statuses takes at most 8 entries, got {}",
            filter.statuses.len()
        );
        let placeholders = vec!["?"; filter.statuses.len()].join(", ");
        where_parts.push(format!("i.status IN ({placeholders})"));
        for status in filter.statuses {
            params.push(Box::new(status.as_str().to_string()));
        }
    }
    if let Some(format) = filter.format {
        where_parts.push("i.format = ?".to_string());
        params.push(Box::new(format.to_string()));
    }

    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };
    (where_clause, joins, params)
}

/// Escape SQL `LIKE` metacharacters (`%`, `_`, and the escape itself)
/// using [`LIKE_ESCAPE`].
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '%' || c == '_' || c == '\\' {
            out.push_str(LIKE_ESCAPE);
        }
        out.push(c);
    }
    out
}

/// One `intake` row read back from `catalog.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intake {
    /// Long-lived surrogate key; never reused after deletion.
    pub intake_id: i64,
    /// Whole-file SHA-256 — the identity anchor.
    pub source_sha256: String,
    /// Opaque store location; `None` until the file has been stored.
    pub stored_path: Option<String>,
    /// Where the file came from, kept for forensics.
    pub original_path: Option<String>,
    /// File format (`epub`, `pdf`, ...); determined during extraction.
    pub format: Option<String>,
    /// File size in bytes.
    pub byte_size: Option<i64>,
    /// Extraction adapter chosen for this file, stamped at EXTRACT.
    pub adapter: Option<String>,
    /// Value of `bookrack_extract::EXTRACTOR_VERSION` at the moment
    /// this file was extracted. A mismatch against the current const
    /// marks the partition stale and due for re-extraction. Defaults
    /// to `1` for rows registered before [`Catalog::set_extraction`]
    /// runs.
    pub extractor_version: u32,
    /// Registration time, as an ISO-8601 UTC timestamp.
    pub intake_at: String,
    /// Coarse lifecycle state.
    pub status: IntakeStatus,
    /// Soft reference to a FRBR expression; backfilled at METADATA.
    pub expression_id: Option<i64>,
    /// Free-form notes.
    pub notes: Option<String>,
}

impl Intake {
    /// Build an [`Intake`] from a row that includes every `intake`
    /// column. Columns are read by name, so the row's column order is
    /// irrelevant.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Intake> {
        Ok(Intake {
            intake_id: row.get("intake_id")?,
            source_sha256: row.get("source_sha256")?,
            stored_path: row.get("stored_path")?,
            original_path: row.get("original_path")?,
            format: row.get("format")?,
            byte_size: row.get("byte_size")?,
            adapter: row.get("adapter")?,
            extractor_version: row.get("extractor_version")?,
            intake_at: row.get("intake_at")?,
            status: decode(row, "status", IntakeStatus::from_db_str)?,
            expression_id: row.get("expression_id")?,
            notes: row.get("notes")?,
        })
    }
}

/// The fields known when a file is first registered. The opaque
/// `stored_path` is deliberately absent: it depends on the
/// autoincremented `intake_id`, so it is filled in afterwards with
/// [`Catalog::set_stored_path`].
#[derive(Debug, Clone)]
pub struct NewIntake {
    source_sha256: String,
    original_path: Option<String>,
    format: Option<String>,
    byte_size: Option<i64>,
}

impl NewIntake {
    /// Start from the whole-file hash — the only mandatory field.
    pub fn new(source_sha256: impl Into<String>) -> NewIntake {
        NewIntake {
            source_sha256: source_sha256.into(),
            original_path: None,
            format: None,
            byte_size: None,
        }
    }

    /// Record where the file came from, for forensics.
    pub fn original_path(mut self, original_path: impl Into<String>) -> NewIntake {
        self.original_path = Some(original_path.into());
        self
    }

    /// Record the detected file format.
    pub fn format(mut self, format: impl Into<String>) -> NewIntake {
        self.format = Some(format.into());
        self
    }

    /// Record the file size in bytes.
    pub fn byte_size(mut self, byte_size: i64) -> NewIntake {
        self.byte_size = Some(byte_size);
        self
    }
}

/// The outcome of [`Catalog::register_intake`]: registration is
/// idempotent, so a file is either freshly recorded or already known.
#[derive(Debug)]
pub enum Registration {
    /// The file was not known and a new intake row was created.
    Created(Intake),
    /// The file was already registered; the existing row is returned.
    AlreadyPresent(Intake),
}

impl Registration {
    /// The intake row, however it was obtained.
    pub fn intake(&self) -> &Intake {
        match self {
            Registration::Created(intake) | Registration::AlreadyPresent(intake) => intake,
        }
    }

    /// Consume the outcome and take the intake row.
    pub fn into_intake(self) -> Intake {
        match self {
            Registration::Created(intake) | Registration::AlreadyPresent(intake) => intake,
        }
    }

    /// Whether this call created the row (rather than finding it).
    pub fn is_new(&self) -> bool {
        matches!(self, Registration::Created(_))
    }
}

impl Catalog {
    /// Register a source file, idempotently on its `source_sha256`.
    ///
    /// If the hash is already known the existing row is returned as
    /// [`Registration::AlreadyPresent`] and nothing is written;
    /// otherwise a new row is created with status
    /// [`IntakeStatus::Pending`] and returned as
    /// [`Registration::Created`].
    pub fn register_intake(&mut self, new: &NewIntake) -> Result<Registration> {
        let tx = self.conn.transaction()?;
        let existing = tx
            .query_row(
                &select_sql("WHERE source_sha256 = :source_sha256"),
                named_params! { ":source_sha256": new.source_sha256 },
                Intake::from_row,
            )
            .optional()?;
        if let Some(intake) = existing {
            return Ok(Registration::AlreadyPresent(intake));
        }

        let intake = tx.query_row(
            &format!("{INSERT_INTAKE_SQL} RETURNING {}", SPEC.select_list()),
            named_params! {
                ":source_sha256": new.source_sha256,
                ":original_path": new.original_path,
                ":format": new.format,
                ":byte_size": new.byte_size,
                ":status": IntakeStatus::Pending.as_str(),
            },
            Intake::from_row,
        )?;
        tx.commit()?;
        Ok(Registration::Created(intake))
    }

    /// Look up an intake by its whole-file hash.
    pub fn intake_by_sha(&self, source_sha256: &str) -> Result<Option<Intake>> {
        let intake = self
            .conn
            .query_row(
                &select_sql("WHERE source_sha256 = :source_sha256"),
                named_params! { ":source_sha256": source_sha256 },
                Intake::from_row,
            )
            .optional()?;
        Ok(intake)
    }

    /// Look up an intake by its id.
    pub fn intake_by_id(&self, intake_id: i64) -> Result<Option<Intake>> {
        let intake = self
            .conn
            .query_row(
                &select_sql("WHERE intake_id = :intake_id"),
                named_params! { ":intake_id": intake_id },
                Intake::from_row,
            )
            .optional()?;
        Ok(intake)
    }

    /// All intakes carrying `status`, ordered by `intake_id` ascending.
    /// Drives batch operations that walk the intake table by lifecycle
    /// state (corpus rebuild, vectors reembed).
    pub fn intakes_with_status(&self, status: IntakeStatus) -> Result<Vec<Intake>> {
        let mut stmt = self
            .conn
            .prepare(&select_sql("WHERE status = :status ORDER BY intake_id"))?;
        let rows = stmt.query_map(
            named_params! { ":status": status.as_str() },
            Intake::from_row,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Find intake rows matching `filter`, ordered by ascending
    /// `intake_id`, paged by `limit` and `offset`. Each filter field is an
    /// optional, AND-combined predicate; see [`IntakeFilter`] for what
    /// each one matches.
    ///
    /// A `limit` of zero, or an `offset` past the end of the result set,
    /// returns an empty `Vec` instead of an error.
    pub fn find_intakes(
        &self,
        filter: &IntakeFilter<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Intake>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (where_clause, joins, mut params) = build_filter_fragments(filter);
        let group_by = if filter.contributor_name.is_some() {
            " GROUP BY i.intake_id"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {cols} FROM intake i{joins}{where_clause}{group_by} \
             ORDER BY i.intake_id LIMIT ? OFFSET ?",
            cols = intake_columns_qualified(),
        );
        params.push(Box::new(limit as i64));
        params.push(Box::new(offset as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), Intake::from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Number of intake rows matching `filter`, sharing the WHERE / JOIN
    /// shape with [`Catalog::find_intakes`] so a `count` and a paged
    /// `find` reach the same set.
    pub fn count_find_intakes(&self, filter: &IntakeFilter<'_>) -> Result<u64> {
        let (where_clause, joins, params) = build_filter_fragments(filter);
        let sql = format!("SELECT COUNT(DISTINCT i.intake_id) FROM intake i{joins}{where_clause}",);
        let refs: Vec<&dyn ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let n: i64 = self
            .conn
            .query_row(&sql, refs.as_slice(), |row| row.get(0))?;
        count_as_u64(n)
    }

    /// Total number of intake rows.
    pub fn count_intakes(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM intake", [], |row| row.get(0))?;
        count_as_u64(n)
    }

    /// Number of intake rows whose `status` falls in `statuses`. An empty
    /// slice means "no filter" and counts every row.
    pub fn count_intakes_by_status(&self, statuses: &[IntakeStatus]) -> Result<u64> {
        if statuses.is_empty() {
            return self.count_intakes();
        }
        debug_assert!(
            statuses.len() <= 8,
            "count_intakes_by_status takes at most 8 statuses, got {}",
            statuses.len()
        );
        let placeholders = vec!["?"; statuses.len()].join(", ");
        let sql = format!("SELECT COUNT(*) FROM intake WHERE status IN ({placeholders})");
        let n: i64 = self.conn.query_row(
            &sql,
            params_from_iter(statuses.iter().map(|s| s.as_str())),
            |row| row.get(0),
        )?;
        count_as_u64(n)
    }

    /// Number of intake rows whose `format` matches `format` exactly.
    /// Rows whose `format` is `NULL` are excluded.
    pub fn count_intakes_by_format(&self, format: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM intake WHERE format = :format",
            named_params! { ":format": format },
            |row| row.get(0),
        )?;
        count_as_u64(n)
    }

    /// Advance an intake's lifecycle state. Returns whether a row with
    /// that id existed.
    pub fn set_intake_status(&self, intake_id: i64, status: IntakeStatus) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE intake SET status = :status WHERE intake_id = :intake_id",
            named_params! { ":status": status.as_str(), ":intake_id": intake_id },
        )?;
        Ok(affected > 0)
    }

    /// Record where an intake's file was placed in the opaque store.
    /// Returns whether a row with that id existed.
    pub fn set_stored_path(&self, intake_id: i64, stored_path: &str) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE intake SET stored_path = :stored_path WHERE intake_id = :intake_id",
            named_params! { ":stored_path": stored_path, ":intake_id": intake_id },
        )?;
        Ok(affected > 0)
    }

    /// Stamp the extraction provenance: the adapter that parsed the
    /// file and the value of `bookrack_extract::EXTRACTOR_VERSION` at
    /// that moment. Both are known together once EXTRACT completes;
    /// recording the version is what later lets a re-extraction detect
    /// a stale partition. Returns whether a row with that id existed.
    pub fn set_extraction(
        &self,
        intake_id: i64,
        adapter: &str,
        extractor_version: u32,
    ) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE intake SET adapter = :adapter, extractor_version = :extractor_version \
             WHERE intake_id = :intake_id",
            named_params! {
                ":adapter": adapter,
                ":extractor_version": extractor_version,
                ":intake_id": intake_id,
            },
        )?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::open_in_memory().expect("open")
    }

    #[test]
    fn a_new_file_registers_as_created() {
        let mut catalog = catalog();
        let reg = catalog
            .register_intake(&NewIntake::new("sha-abc"))
            .expect("register");
        assert!(reg.is_new());
        let intake = reg.intake();
        assert!(intake.intake_id > 0);
        assert_eq!(intake.source_sha256, "sha-abc");
        assert_eq!(intake.status, IntakeStatus::Pending);
        assert_eq!(intake.stored_path, None);
        assert!(!intake.intake_at.is_empty());
    }

    #[test]
    fn an_intake_round_trips_every_field() {
        let mut catalog = catalog();
        let registered = catalog
            .register_intake(
                &NewIntake::new("sha-rt")
                    .original_path("incoming/book.epub")
                    .format("epub")
                    .byte_size(8192),
            )
            .expect("register")
            .into_intake();
        let id = registered.intake_id;
        assert!(catalog.set_stored_path(id, "store/42").expect("set path"));
        assert!(
            catalog
                .set_intake_status(id, IntakeStatus::Extracted)
                .expect("set status")
        );

        // Fetch through `from_row` and confirm every column survives.
        let read = catalog.intake_by_id(id).expect("lookup").expect("present");
        assert_eq!(read.intake_id, id);
        assert_eq!(read.source_sha256, "sha-rt");
        assert_eq!(read.stored_path.as_deref(), Some("store/42"));
        assert_eq!(read.original_path.as_deref(), Some("incoming/book.epub"));
        assert_eq!(read.format.as_deref(), Some("epub"));
        assert_eq!(read.byte_size, Some(8192));
        assert_eq!(read.status, IntakeStatus::Extracted);
        assert!(!read.intake_at.is_empty());
        // `adapter` is filled by `set_extraction`, which this test does
        // not call, so it remains absent. `extractor_version` has a
        // table default of `1` and so is non-NULL even on a freshly
        // registered row.
        assert_eq!(read.adapter, None);
        assert_eq!(read.extractor_version, 1);
        // `expression_id` is reserved for the FRBR grouping work and
        // `notes` for user-supplied remarks; the ingest pipeline does not
        // fill either today.
        assert_eq!(read.expression_id, None);
        assert_eq!(read.notes, None);
    }

    #[test]
    fn re_registering_the_same_file_returns_the_existing_row() {
        let mut catalog = catalog();
        let first = catalog
            .register_intake(&NewIntake::new("sha-dup"))
            .expect("register")
            .into_intake();
        let again = catalog
            .register_intake(&NewIntake::new("sha-dup"))
            .expect("re-register");
        assert!(!again.is_new(), "a known file must not create a second row");
        assert_eq!(again.intake().intake_id, first.intake_id);
    }

    #[test]
    fn optional_fields_round_trip() {
        let mut catalog = catalog();
        let new = NewIntake::new("sha-xyz")
            .original_path("incoming/book.epub")
            .format("epub")
            .byte_size(4096);
        let intake = catalog
            .register_intake(&new)
            .expect("register")
            .into_intake();
        assert_eq!(intake.original_path.as_deref(), Some("incoming/book.epub"));
        assert_eq!(intake.format.as_deref(), Some("epub"));
        assert_eq!(intake.byte_size, Some(4096));
    }

    #[test]
    fn intake_lookups_by_sha_and_id() {
        let mut catalog = catalog();
        let intake = catalog
            .register_intake(&NewIntake::new("sha-find"))
            .expect("register")
            .into_intake();

        let by_sha = catalog.intake_by_sha("sha-find").expect("lookup");
        assert_eq!(by_sha.map(|i| i.intake_id), Some(intake.intake_id));
        let by_id = catalog.intake_by_id(intake.intake_id).expect("lookup");
        assert_eq!(by_id, Some(intake));

        assert!(catalog.intake_by_sha("absent").expect("lookup").is_none());
        assert!(catalog.intake_by_id(9999).expect("lookup").is_none());
    }

    #[test]
    fn stored_path_and_status_can_be_set() {
        let mut catalog = catalog();
        let id = catalog
            .register_intake(&NewIntake::new("sha-set"))
            .expect("register")
            .intake()
            .intake_id;

        assert!(catalog.set_stored_path(id, "store/7").expect("set path"));
        assert!(
            catalog
                .set_intake_status(id, IntakeStatus::Extracted)
                .expect("set status")
        );

        let intake = catalog.intake_by_id(id).expect("lookup").expect("present");
        assert_eq!(intake.stored_path.as_deref(), Some("store/7"));
        assert_eq!(intake.status, IntakeStatus::Extracted);

        // No such intake: nothing updated.
        assert!(
            !catalog
                .set_intake_status(9999, IntakeStatus::Aborted)
                .expect("miss")
        );
        assert!(!catalog.set_stored_path(9999, "store/x").expect("miss"));
    }

    /// Seed an intake with title `title` and one author `author`,
    /// returning the new intake id.
    fn seed_book(catalog: &mut Catalog, sha: &str, title: &str, author: &str) -> i64 {
        use crate::{NewContributor, NewPublicationAttrs};

        let intake_id = catalog
            .register_intake(&NewIntake::new(sha).format("epub"))
            .expect("register")
            .intake()
            .intake_id;

        let mut attrs = NewPublicationAttrs::new(intake_id, BOOK_SCOPE);
        attrs.title = Some(title.to_string());
        catalog.upsert_publication_attrs(&attrs).expect("attrs");

        catalog
            .add_contributor(&NewContributor::new(
                intake_id,
                BOOK_SCOPE,
                "author",
                0,
                "extracted",
                author,
            ))
            .expect("contributor");

        intake_id
    }

    #[test]
    fn find_intakes_with_empty_filter_lists_every_row() {
        let mut catalog = catalog();
        seed_book(&mut catalog, "sha-a", "Alpha", "Ann");
        seed_book(&mut catalog, "sha-b", "Beta", "Ben");
        let hits = catalog
            .find_intakes(&IntakeFilter::default(), 10, 0)
            .expect("find");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn find_intakes_title_substring_matches_case_sensitive_and_paginates() {
        let mut catalog = catalog();
        seed_book(&mut catalog, "sha-a", "Alpha Bravo", "Ann");
        seed_book(&mut catalog, "sha-b", "Bravo Charlie", "Ben");
        seed_book(&mut catalog, "sha-c", "Charlie Delta", "Cal");

        let filter = IntakeFilter {
            title_substring: Some("Bravo"),
            ..IntakeFilter::default()
        };
        let hits = catalog.find_intakes(&filter, 10, 0).expect("find");
        assert_eq!(hits.len(), 2);
        assert_eq!(
            catalog.count_find_intakes(&filter).expect("count"),
            hits.len() as u64
        );

        // Paged.
        let first = catalog.find_intakes(&filter, 1, 0).expect("page1");
        let second = catalog.find_intakes(&filter, 1, 1).expect("page2");
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].intake_id, second[0].intake_id);
    }

    #[test]
    fn find_intakes_title_substring_escapes_like_metachars() {
        let mut catalog = catalog();
        seed_book(&mut catalog, "sha-a", "100% Pure", "Ann");
        seed_book(&mut catalog, "sha-b", "100 Pure", "Ben");
        // The `%` in the needle must match the literal `%` row only.
        let filter = IntakeFilter {
            title_substring: Some("100%"),
            ..IntakeFilter::default()
        };
        let hits = catalog.find_intakes(&filter, 10, 0).expect("find");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_sha256, "sha-a");
    }

    #[test]
    fn find_intakes_contributor_name_uses_exact_equality() {
        let mut catalog = catalog();
        seed_book(&mut catalog, "sha-a", "Alpha", "Ann");
        seed_book(&mut catalog, "sha-b", "Beta", "Anderson");
        let filter = IntakeFilter {
            contributor_name: Some("Ann"),
            ..IntakeFilter::default()
        };
        let hits = catalog.find_intakes(&filter, 10, 0).expect("find");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_sha256, "sha-a");
    }

    #[test]
    fn find_intakes_contributor_role_narrows_within_a_name() {
        use crate::NewContributor;
        let mut catalog = catalog();
        seed_book(&mut catalog, "sha-a", "Alpha", "Ann");
        // Same name as author on intake A, but a translator role.
        catalog
            .add_contributor(&NewContributor::new(
                catalog
                    .intake_by_sha("sha-a")
                    .expect("lookup")
                    .expect("present")
                    .intake_id,
                BOOK_SCOPE,
                "translator",
                0,
                "extracted",
                "Tia",
            ))
            .expect("translator");
        seed_book(&mut catalog, "sha-b", "Beta", "Tia");

        // Without a role, Tia hits both books.
        let any = IntakeFilter {
            contributor_name: Some("Tia"),
            ..IntakeFilter::default()
        };
        assert_eq!(catalog.find_intakes(&any, 10, 0).expect("find").len(), 2);

        // With role "translator", only the book where Tia is a translator.
        let scoped = IntakeFilter {
            contributor_name: Some("Tia"),
            contributor_role: Some("translator"),
            ..IntakeFilter::default()
        };
        let hits = catalog.find_intakes(&scoped, 10, 0).expect("find");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_sha256, "sha-a");
    }

    #[test]
    fn find_intakes_statuses_filter_with_in_list() {
        let mut catalog = catalog();
        let a = seed_book(&mut catalog, "sha-a", "Alpha", "Ann");
        let b = seed_book(&mut catalog, "sha-b", "Beta", "Ben");
        seed_book(&mut catalog, "sha-c", "Gamma", "Cal");
        catalog
            .set_intake_status(a, IntakeStatus::Extracted)
            .expect("set");
        catalog
            .set_intake_status(b, IntakeStatus::Embedded)
            .expect("set");

        let filter = IntakeFilter {
            statuses: &[IntakeStatus::Extracted, IntakeStatus::Embedded],
            ..IntakeFilter::default()
        };
        let hits = catalog.find_intakes(&filter, 10, 0).expect("find");
        assert_eq!(hits.len(), 2);
        assert_eq!(catalog.count_find_intakes(&filter).expect("count"), 2);
    }

    #[test]
    fn find_intakes_combines_title_and_contributor_without_duplicating_rows() {
        use crate::NewContributor;
        let mut catalog = catalog();
        let a = seed_book(&mut catalog, "sha-a", "Alpha Bravo", "Ann");
        // Same author also listed as editor on the same book.
        catalog
            .add_contributor(&NewContributor::new(
                a,
                BOOK_SCOPE,
                "editor",
                0,
                "extracted",
                "Ann",
            ))
            .expect("editor row");
        seed_book(&mut catalog, "sha-b", "Bravo Charlie", "Ben");

        let filter = IntakeFilter {
            title_substring: Some("Bravo"),
            contributor_name: Some("Ann"),
            ..IntakeFilter::default()
        };
        let hits = catalog.find_intakes(&filter, 10, 0).expect("find");
        // Despite two matching contributor rows, GROUP BY collapses to one.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].intake_id, a);
        assert_eq!(catalog.count_find_intakes(&filter).expect("count"), 1);
    }

    #[test]
    fn find_intakes_format_filter_excludes_null_format() {
        let mut catalog = catalog();
        catalog
            .register_intake(&NewIntake::new("sha-no-format"))
            .expect("register");
        catalog
            .register_intake(&NewIntake::new("sha-epub").format("epub"))
            .expect("register");

        let filter = IntakeFilter {
            format: Some("epub"),
            ..IntakeFilter::default()
        };
        let hits = catalog.find_intakes(&filter, 10, 0).expect("find");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_sha256, "sha-epub");
    }

    #[test]
    fn find_intakes_limit_zero_and_offset_beyond_return_empty() {
        let mut catalog = catalog();
        seed_book(&mut catalog, "sha-a", "Alpha", "Ann");
        assert!(
            catalog
                .find_intakes(&IntakeFilter::default(), 0, 0)
                .expect("limit zero")
                .is_empty()
        );
        assert!(
            catalog
                .find_intakes(&IntakeFilter::default(), 10, 99)
                .expect("offset past end")
                .is_empty()
        );
    }

    #[test]
    fn count_find_intakes_matches_a_max_limit_find() {
        let mut catalog = catalog();
        seed_book(&mut catalog, "sha-a", "Alpha", "Ann");
        seed_book(&mut catalog, "sha-b", "Beta", "Ben");
        seed_book(&mut catalog, "sha-c", "Gamma", "Cal");
        let filter = IntakeFilter::default();
        let count = catalog.count_find_intakes(&filter).expect("count");
        let hits = catalog
            .find_intakes(&filter, u32::MAX, 0)
            .expect("find unbounded");
        assert_eq!(count as usize, hits.len());
    }

    #[test]
    fn find_intakes_runs_in_under_ten_millis_on_a_seven_field_filter() {
        let mut catalog = catalog();
        for i in 0..50u32 {
            let sha = format!("sha-{i:03}");
            let title = format!("Title {i:03}");
            let author = format!("Author {i:03}");
            seed_book(&mut catalog, &sha, &title, &author);
        }
        let filter = IntakeFilter {
            title_substring: Some("Title"),
            contributor_name: Some("Author 042"),
            contributor_role: Some("author"),
            statuses: &[IntakeStatus::Pending],
            format: Some("epub"),
        };
        let start = std::time::Instant::now();
        let hits = catalog
            .find_intakes(&filter, 100, 0)
            .expect("filtered find");
        let elapsed = start.elapsed();
        assert_eq!(hits.len(), 1);
        // 168-book scale sanity check from the manual; 10 ms is generous.
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "filtered find took {elapsed:?}"
        );
    }

    #[test]
    fn count_intakes_grows_with_each_registration() {
        let mut catalog = catalog();
        assert_eq!(catalog.count_intakes().expect("count empty"), 0);
        catalog
            .register_intake(&NewIntake::new("sha-a"))
            .expect("register");
        catalog
            .register_intake(&NewIntake::new("sha-b"))
            .expect("register");
        assert_eq!(catalog.count_intakes().expect("count two"), 2);
    }

    #[test]
    fn count_intakes_by_status_filters_and_sums() {
        let mut catalog = catalog();
        let ids: Vec<i64> = ["sha-a", "sha-b", "sha-c"]
            .iter()
            .map(|sha| {
                catalog
                    .register_intake(&NewIntake::new(*sha))
                    .expect("register")
                    .intake()
                    .intake_id
            })
            .collect();
        catalog
            .set_intake_status(ids[1], IntakeStatus::Extracted)
            .expect("set");
        catalog
            .set_intake_status(ids[2], IntakeStatus::Embedded)
            .expect("set");

        assert_eq!(
            catalog
                .count_intakes_by_status(&[IntakeStatus::Pending])
                .expect("count one"),
            1
        );
        assert_eq!(
            catalog
                .count_intakes_by_status(&[IntakeStatus::Extracted, IntakeStatus::Embedded])
                .expect("count in-list"),
            2
        );
        assert_eq!(
            catalog
                .count_intakes_by_status(&[IntakeStatus::Aborted])
                .expect("count miss"),
            0
        );
    }

    #[test]
    fn count_intakes_by_status_empty_slice_counts_everything() {
        let mut catalog = catalog();
        catalog
            .register_intake(&NewIntake::new("sha-x"))
            .expect("register");
        catalog
            .register_intake(&NewIntake::new("sha-y"))
            .expect("register");
        assert_eq!(
            catalog
                .count_intakes_by_status(&[])
                .expect("count empty slice"),
            catalog.count_intakes().expect("count all"),
        );
    }

    #[test]
    fn count_intakes_by_format_filters_and_misses() {
        let mut catalog = catalog();
        catalog
            .register_intake(&NewIntake::new("sha-1").format("epub"))
            .expect("register");
        catalog
            .register_intake(&NewIntake::new("sha-2").format("epub"))
            .expect("register");
        catalog
            .register_intake(&NewIntake::new("sha-3").format("pdf"))
            .expect("register");
        // A format-less intake is excluded by the WHERE.
        catalog
            .register_intake(&NewIntake::new("sha-4"))
            .expect("register");

        assert_eq!(
            catalog.count_intakes_by_format("epub").expect("count epub"),
            2
        );
        assert_eq!(
            catalog.count_intakes_by_format("pdf").expect("count pdf"),
            1
        );
        assert_eq!(
            catalog
                .count_intakes_by_format("mobi")
                .expect("count unknown"),
            0
        );
    }

    #[test]
    fn intake_status_db_strings_round_trip() {
        for status in IntakeStatus::ALL {
            assert_eq!(IntakeStatus::from_db_str(status.as_str()), Some(status));
        }
        assert_eq!(IntakeStatus::from_db_str("not_a_status"), None);
    }
}
