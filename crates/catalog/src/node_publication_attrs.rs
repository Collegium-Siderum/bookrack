// SPDX-License-Identifier: Apache-2.0

//! The `node_publication_attrs` table — the metadata base layer.
//!
//! One row per node carrying the bibliographic attributes as extracted
//! (or enriched). This is the *base* layer: user corrections live in
//! `node_overrides` and are applied on top by the effective-metadata
//! merge. The row is keyed by the logical address `(intake_id, scope)`
//! and rewritten as a unit.

use bookrack_dbkit::{ColumnSpec, TableSpec};
use rusqlite::{OptionalExtension, Row, named_params};

use crate::{Catalog, Result};

/// The single source of truth for the `node_publication_attrs` table's
/// schema. Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "node_publication_attrs",
    comment: Some("Extracted bibliographic attributes — the metadata base layer."),
    columns: &[
        ColumnSpec::int("intake_id").not_null(),
        ColumnSpec::text("scope").not_null(),
        ColumnSpec::text("title"),
        ColumnSpec::text("subtitle"),
        ColumnSpec::text("publisher"),
        ColumnSpec::text("year"),
        ColumnSpec::text("publication_date"),
        ColumnSpec::text("isbn"),
        ColumnSpec::text("series"),
        ColumnSpec::text("series_number"),
        ColumnSpec::text("edition"),
        ColumnSpec::text("language"),
        ColumnSpec::text("pub_place")
            .comment("city of publication; the GB/T 7714 and Chicago bibliography styles need it"),
        ColumnSpec::text("original_title")
            .comment("pre-FRBR stopgap: a translation's original-language title"),
        ColumnSpec::text("original_language")
            .comment("pre-FRBR stopgap: the work's original language"),
        ColumnSpec::text("original_year")
            .comment("pre-FRBR stopgap: a translation's original-language publication year"),
        ColumnSpec::text("source_format"),
        ColumnSpec::text("source").comment("ocr_marker / extracted / web"),
        ColumnSpec::text("confidence").comment("high / medium / low"),
        ColumnSpec::text("audit_verdict").comment("clean / needs_work"),
        ColumnSpec::text("enriched_by"),
    ],
    composite_pk: Some(&["intake_id", "scope"]),
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

/// Insert or replace a node's base-layer attributes, keyed by the logical
/// address `(intake_id, scope)`.
const UPSERT_SQL: &str = "INSERT INTO node_publication_attrs \
     (intake_id, scope, title, subtitle, publisher, year, publication_date, isbn, series, \
      series_number, edition, language, pub_place, original_title, original_language, \
      original_year, source_format, source, confidence, audit_verdict, enriched_by) \
     VALUES (:intake_id, :scope, :title, :subtitle, :publisher, :year, :publication_date, \
             :isbn, :series, :series_number, :edition, :language, :pub_place, \
             :original_title, :original_language, :original_year, :source_format, :source, \
             :confidence, :audit_verdict, :enriched_by) \
     ON CONFLICT(intake_id, scope) DO UPDATE SET \
       title = excluded.title, \
       subtitle = excluded.subtitle, \
       publisher = excluded.publisher, \
       year = excluded.year, \
       publication_date = excluded.publication_date, \
       isbn = excluded.isbn, \
       series = excluded.series, \
       series_number = excluded.series_number, \
       edition = excluded.edition, \
       language = excluded.language, \
       pub_place = excluded.pub_place, \
       original_title = excluded.original_title, \
       original_language = excluded.original_language, \
       original_year = excluded.original_year, \
       source_format = excluded.source_format, \
       source = excluded.source, \
       confidence = excluded.confidence, \
       audit_verdict = excluded.audit_verdict, \
       enriched_by = excluded.enriched_by";

/// A `SELECT` of every column with `tail` (a `WHERE` clause) appended.
/// The column list is derived from [`SPEC`].
fn select_sql(tail: &str) -> String {
    format!(
        "SELECT {} FROM node_publication_attrs {tail}",
        SPEC.select_list()
    )
}

/// One `node_publication_attrs` row — a node's extracted bibliographic
/// attributes. Every attribute is optional; a node need not carry them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationAttrs {
    /// The book whose node these attributes describe — a soft reference
    /// to the `intake` registry.
    pub intake_id: i64,
    /// The logical address of the node within the book's partition.
    pub scope: String,
    /// Main title.
    pub title: Option<String>,
    /// Subtitle.
    pub subtitle: Option<String>,
    /// Publisher.
    pub publisher: Option<String>,
    /// Publication year, as text (sources vary in precision).
    pub year: Option<String>,
    /// Full publication date, when known.
    pub publication_date: Option<String>,
    /// ISBN.
    pub isbn: Option<String>,
    /// Series the work belongs to.
    pub series: Option<String>,
    /// Position within the series.
    pub series_number: Option<String>,
    /// Edition.
    pub edition: Option<String>,
    /// Language of this manifestation.
    pub language: Option<String>,
    /// City of publication. Required by GB/T 7714 / Chicago bibliography styles.
    pub pub_place: Option<String>,
    /// A translation's original-language title — a pre-FRBR stopgap.
    pub original_title: Option<String>,
    /// The work's original language — a pre-FRBR stopgap.
    pub original_language: Option<String>,
    /// A translation's original-language publication year — a pre-FRBR stopgap.
    pub original_year: Option<String>,
    /// Format of the source the attributes were drawn from.
    pub source_format: Option<String>,
    /// Where the attributes came from (`ocr_marker` / `extracted` / `web`).
    pub source: Option<String>,
    /// Extraction confidence (`high` / `medium` / `low`).
    pub confidence: Option<String>,
    /// Plausibility verdict from the metadata audit (`clean` /
    /// `needs_work`). Stamped at ingest alongside `confidence` so a
    /// later `metadata show` can render the historical audit outcome
    /// without re-running the audit on synthetic inputs.
    pub audit_verdict: Option<String>,
    /// What produced the enrichment, when the row was enriched.
    pub enriched_by: Option<String>,
}

impl PublicationAttrs {
    /// Build a [`PublicationAttrs`] from a row that includes every
    /// column. Columns are read by name.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<PublicationAttrs> {
        Ok(PublicationAttrs {
            intake_id: row.get("intake_id")?,
            scope: row.get("scope")?,
            title: row.get("title")?,
            subtitle: row.get("subtitle")?,
            publisher: row.get("publisher")?,
            year: row.get("year")?,
            publication_date: row.get("publication_date")?,
            isbn: row.get("isbn")?,
            series: row.get("series")?,
            series_number: row.get("series_number")?,
            edition: row.get("edition")?,
            language: row.get("language")?,
            pub_place: row.get("pub_place")?,
            original_title: row.get("original_title")?,
            original_language: row.get("original_language")?,
            original_year: row.get("original_year")?,
            source_format: row.get("source_format")?,
            source: row.get("source")?,
            confidence: row.get("confidence")?,
            audit_verdict: row.get("audit_verdict")?,
            enriched_by: row.get("enriched_by")?,
        })
    }
}

/// The base-layer attributes about to be written for one node.
///
/// Start from [`NewPublicationAttrs::new`] and set the attributes that
/// were extracted; the rest stay absent. This is a flat record written
/// as a unit, so its attributes are public fields rather than builder
/// methods.
#[derive(Debug, Clone)]
pub struct NewPublicationAttrs {
    /// The book whose node these attributes describe.
    pub intake_id: i64,
    /// The logical address of the node within the book's partition.
    pub scope: String,
    /// Main title.
    pub title: Option<String>,
    /// Subtitle.
    pub subtitle: Option<String>,
    /// Publisher.
    pub publisher: Option<String>,
    /// Publication year.
    pub year: Option<String>,
    /// Full publication date.
    pub publication_date: Option<String>,
    /// ISBN.
    pub isbn: Option<String>,
    /// Series.
    pub series: Option<String>,
    /// Position within the series.
    pub series_number: Option<String>,
    /// Edition.
    pub edition: Option<String>,
    /// Language of this manifestation.
    pub language: Option<String>,
    /// City of publication.
    pub pub_place: Option<String>,
    /// A translation's original-language title.
    pub original_title: Option<String>,
    /// The work's original language.
    pub original_language: Option<String>,
    /// A translation's original-language publication year.
    pub original_year: Option<String>,
    /// Format of the source.
    pub source_format: Option<String>,
    /// Where the attributes came from.
    pub source: Option<String>,
    /// Extraction confidence.
    pub confidence: Option<String>,
    /// Audit verdict (`clean` / `needs_work`) — see
    /// [`PublicationAttrs::audit_verdict`].
    pub audit_verdict: Option<String>,
    /// What produced the enrichment.
    pub enriched_by: Option<String>,
}

impl NewPublicationAttrs {
    /// A record for the node at `(intake_id, scope)` with every attribute
    /// absent.
    pub fn new(intake_id: i64, scope: impl Into<String>) -> NewPublicationAttrs {
        NewPublicationAttrs {
            intake_id,
            scope: scope.into(),
            title: None,
            subtitle: None,
            publisher: None,
            year: None,
            publication_date: None,
            isbn: None,
            series: None,
            series_number: None,
            edition: None,
            language: None,
            pub_place: None,
            original_title: None,
            original_language: None,
            original_year: None,
            source_format: None,
            source: None,
            confidence: None,
            audit_verdict: None,
            enriched_by: None,
        }
    }
}

impl Catalog {
    /// Insert or replace a node's base-layer bibliographic attributes.
    pub fn upsert_publication_attrs(&self, new: &NewPublicationAttrs) -> Result<()> {
        self.conn.execute(
            UPSERT_SQL,
            named_params! {
                ":intake_id": new.intake_id,
                ":scope": new.scope,
                ":title": new.title,
                ":subtitle": new.subtitle,
                ":publisher": new.publisher,
                ":year": new.year,
                ":publication_date": new.publication_date,
                ":isbn": new.isbn,
                ":series": new.series,
                ":series_number": new.series_number,
                ":edition": new.edition,
                ":language": new.language,
                ":pub_place": new.pub_place,
                ":original_title": new.original_title,
                ":original_language": new.original_language,
                ":original_year": new.original_year,
                ":source_format": new.source_format,
                ":source": new.source,
                ":confidence": new.confidence,
                ":audit_verdict": new.audit_verdict,
                ":enriched_by": new.enriched_by,
            },
        )?;
        Ok(())
    }

    /// Fetch the base-layer attributes at `(intake_id, scope)`, or `None`
    /// if none exist.
    pub fn publication_attrs(
        &self,
        intake_id: i64,
        scope: &str,
    ) -> Result<Option<PublicationAttrs>> {
        let attrs = self
            .conn
            .query_row(
                &select_sql("WHERE intake_id = :intake_id AND scope = :scope"),
                named_params! { ":intake_id": intake_id, ":scope": scope },
                PublicationAttrs::from_row,
            )
            .optional()?;
        Ok(attrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A logical address used throughout these tests.
    const SCOPE: &str = "book";

    /// A `NewPublicationAttrs` with every attribute set to a distinct
    /// value, so a dropped column or unbound parameter fails a test.
    fn fully_populated(intake_id: i64, scope: &str) -> NewPublicationAttrs {
        NewPublicationAttrs {
            intake_id,
            scope: scope.into(),
            title: Some("Title".into()),
            subtitle: Some("Subtitle".into()),
            publisher: Some("Publisher".into()),
            year: Some("1990".into()),
            publication_date: Some("1990-06-01".into()),
            isbn: Some("978-0-00-000000-0".into()),
            series: Some("Series".into()),
            series_number: Some("3".into()),
            edition: Some("2nd".into()),
            language: Some("en".into()),
            pub_place: Some("New York".into()),
            original_title: Some("Original Title".into()),
            original_language: Some("fr".into()),
            original_year: Some("1962".into()),
            source_format: Some("epub".into()),
            source: Some("extracted".into()),
            confidence: Some("high".into()),
            audit_verdict: Some("clean".into()),
            enriched_by: Some("llm".into()),
        }
    }

    #[test]
    fn publication_attrs_round_trip_every_field() {
        let catalog = Catalog::open_in_memory().expect("open");
        catalog
            .upsert_publication_attrs(&fully_populated(1, SCOPE))
            .expect("write");

        let read = catalog
            .publication_attrs(1, SCOPE)
            .expect("read")
            .expect("present");
        assert_eq!(read.intake_id, 1);
        assert_eq!(read.scope, SCOPE);
        assert_eq!(read.title.as_deref(), Some("Title"));
        assert_eq!(read.subtitle.as_deref(), Some("Subtitle"));
        assert_eq!(read.publisher.as_deref(), Some("Publisher"));
        assert_eq!(read.year.as_deref(), Some("1990"));
        assert_eq!(read.publication_date.as_deref(), Some("1990-06-01"));
        assert_eq!(read.isbn.as_deref(), Some("978-0-00-000000-0"));
        assert_eq!(read.series.as_deref(), Some("Series"));
        assert_eq!(read.series_number.as_deref(), Some("3"));
        assert_eq!(read.edition.as_deref(), Some("2nd"));
        assert_eq!(read.language.as_deref(), Some("en"));
        assert_eq!(read.pub_place.as_deref(), Some("New York"));
        assert_eq!(read.original_title.as_deref(), Some("Original Title"));
        assert_eq!(read.original_language.as_deref(), Some("fr"));
        assert_eq!(read.original_year.as_deref(), Some("1962"));
        assert_eq!(read.source_format.as_deref(), Some("epub"));
        assert_eq!(read.source.as_deref(), Some("extracted"));
        assert_eq!(read.confidence.as_deref(), Some("high"));
        assert_eq!(read.audit_verdict.as_deref(), Some("clean"));
        assert_eq!(read.enriched_by.as_deref(), Some("llm"));
    }

    #[test]
    fn a_missing_row_reads_as_none() {
        let catalog = Catalog::open_in_memory().expect("open");
        assert!(
            catalog
                .publication_attrs(404, SCOPE)
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn upsert_overwrites_the_previous_attributes() {
        let catalog = Catalog::open_in_memory().expect("open");
        catalog
            .upsert_publication_attrs(&fully_populated(1, SCOPE))
            .expect("first write");
        let mut revised = NewPublicationAttrs::new(1, SCOPE);
        revised.title = Some("Revised".into());
        catalog
            .upsert_publication_attrs(&revised)
            .expect("second write");

        let read = catalog
            .publication_attrs(1, SCOPE)
            .expect("read")
            .expect("present");
        assert_eq!(read.title.as_deref(), Some("Revised"));
        // A field absent in the second write is cleared, not retained.
        assert_eq!(read.publisher, None);
    }
}
