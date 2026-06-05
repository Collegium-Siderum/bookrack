// SPDX-License-Identifier: Apache-2.0

//! The `book_state` table — book-level pipeline state.
//!
//! One row per ingested book, tracking how far it has progressed through
//! the pipeline. The row is keyed by the book's root node id and is
//! rewritten in place as the book advances.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec};
use rusqlite::{OptionalExtension, Row, named_params};

use crate::{Catalog, Result, count_as_u64};

/// The single source of truth for the `book_state` table's schema. Its
/// DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "book_state",
    comment: Some("Book-level pipeline state, one row per ingested book."),
    columns: &[
        ColumnSpec::int("book_root_id")
            .primary_key()
            .comment("soft reference to corpus.nodes"),
        ColumnSpec::int("intake_id").not_null().unique(),
        ColumnSpec::text("current_stage").not_null(),
        ColumnSpec::text("embed_model"),
        ColumnSpec::text("parsed_at").comment("STRUCTURE completed"),
        ColumnSpec::text("embedded_at").comment("EMBED completed; non-NULL iff vectors exist"),
        ColumnSpec::text("ocr_marker_finished_at"),
        ColumnSpec::text("last_error"),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[
        IndexSpec::on("idx_book_state_stage", &["current_stage"]),
        IndexSpec::on("idx_book_state_embed", &["embedded_at"]).partial("embedded_at IS NULL"),
    ],
};

/// Insert or replace a book's pipeline state. The row is keyed by
/// `book_root_id`, so re-recording a book overwrites its previous state.
/// `ocr_marker_finished_at` is an append-only audit stamp: an update
/// that does not provide a new value keeps the existing one rather
/// than clearing it.
const UPSERT_SQL: &str = "INSERT INTO book_state \
     (book_root_id, intake_id, current_stage, embed_model, parsed_at, \
      embedded_at, ocr_marker_finished_at, last_error) \
     VALUES (:book_root_id, :intake_id, :current_stage, :embed_model, :parsed_at, \
             :embedded_at, :ocr_marker_finished_at, :last_error) \
     ON CONFLICT(book_root_id) DO UPDATE SET \
       intake_id = excluded.intake_id, \
       current_stage = excluded.current_stage, \
       embed_model = excluded.embed_model, \
       parsed_at = excluded.parsed_at, \
       embedded_at = excluded.embedded_at, \
       ocr_marker_finished_at = COALESCE(excluded.ocr_marker_finished_at, ocr_marker_finished_at), \
       last_error = excluded.last_error";

/// A `SELECT` of every column with `tail` (a `WHERE` clause) appended.
/// The column list is derived from [`SPEC`].
fn select_sql(tail: &str) -> String {
    format!("SELECT {} FROM book_state {tail}", SPEC.select_list())
}

/// One `book_state` row read back from `catalog.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookState {
    /// The book's root node id — a soft reference to `corpus.db`.
    pub book_root_id: i64,
    /// The intake this book was ingested from.
    pub intake_id: i64,
    /// The pipeline stage the book currently sits at.
    pub current_stage: String,
    /// The embedding model the book was embedded with, once embedded.
    pub embed_model: Option<String>,
    /// When the STRUCTURE stage completed.
    pub parsed_at: Option<String>,
    /// When the EMBED stage completed; non-`None` iff vectors exist.
    pub embedded_at: Option<String>,
    /// When OCR-marker processing finished, for books that needed it.
    pub ocr_marker_finished_at: Option<String>,
    /// The last error seen while processing this book, if any.
    pub last_error: Option<String>,
}

impl BookState {
    /// Build a [`BookState`] from a row that includes every column.
    /// Columns are read by name, so the row's column order is irrelevant.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<BookState> {
        Ok(BookState {
            book_root_id: row.get("book_root_id")?,
            intake_id: row.get("intake_id")?,
            current_stage: row.get("current_stage")?,
            embed_model: row.get("embed_model")?,
            parsed_at: row.get("parsed_at")?,
            embedded_at: row.get("embedded_at")?,
            ocr_marker_finished_at: row.get("ocr_marker_finished_at")?,
            last_error: row.get("last_error")?,
        })
    }
}

/// A `book_state` row about to be written. Start from [`NewBookState::new`]
/// and attach the optional timestamps and fields with the builder methods.
#[derive(Debug, Clone)]
pub struct NewBookState {
    book_root_id: i64,
    intake_id: i64,
    current_stage: String,
    embed_model: Option<String>,
    parsed_at: Option<String>,
    embedded_at: Option<String>,
    ocr_marker_finished_at: Option<String>,
    last_error: Option<String>,
}

impl NewBookState {
    /// A book at `current_stage`, with every optional field cleared.
    pub fn new(
        book_root_id: i64,
        intake_id: i64,
        current_stage: impl Into<String>,
    ) -> NewBookState {
        NewBookState {
            book_root_id,
            intake_id,
            current_stage: current_stage.into(),
            embed_model: None,
            parsed_at: None,
            embedded_at: None,
            ocr_marker_finished_at: None,
            last_error: None,
        }
    }

    /// Record the embedding model the book was embedded with.
    pub fn embed_model(mut self, value: impl Into<String>) -> NewBookState {
        self.embed_model = Some(value.into());
        self
    }

    /// Record when the STRUCTURE stage completed.
    pub fn parsed_at(mut self, value: impl Into<String>) -> NewBookState {
        self.parsed_at = Some(value.into());
        self
    }

    /// Record when the EMBED stage completed.
    pub fn embedded_at(mut self, value: impl Into<String>) -> NewBookState {
        self.embedded_at = Some(value.into());
        self
    }

    /// Record when OCR-marker processing finished.
    pub fn ocr_marker_finished_at(mut self, value: impl Into<String>) -> NewBookState {
        self.ocr_marker_finished_at = Some(value.into());
        self
    }

    /// Record the last processing error.
    pub fn last_error(mut self, value: impl Into<String>) -> NewBookState {
        self.last_error = Some(value.into());
        self
    }
}

impl Catalog {
    /// Insert or replace a book's pipeline state.
    pub fn upsert_book_state(&self, new: &NewBookState) -> Result<()> {
        self.conn.execute(
            UPSERT_SQL,
            named_params! {
                ":book_root_id": new.book_root_id,
                ":intake_id": new.intake_id,
                ":current_stage": new.current_stage,
                ":embed_model": new.embed_model,
                ":parsed_at": new.parsed_at,
                ":embedded_at": new.embedded_at,
                ":ocr_marker_finished_at": new.ocr_marker_finished_at,
                ":last_error": new.last_error,
            },
        )?;
        Ok(())
    }

    /// Number of book-state rows currently at `stage`. Uses
    /// `idx_book_state_stage`.
    pub fn count_book_states_by_stage(&self, stage: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM book_state WHERE current_stage = :stage",
            named_params! { ":stage": stage },
            |row| row.get(0),
        )?;
        count_as_u64(n)
    }

    /// Fetch a book's pipeline state, or `None` if none is recorded.
    pub fn book_state(&self, book_root_id: i64) -> Result<Option<BookState>> {
        let state = self
            .conn
            .query_row(
                &select_sql("WHERE book_root_id = :book_root_id"),
                named_params! { ":book_root_id": book_root_id },
                BookState::from_row,
            )
            .optional()?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_state_round_trips_every_field() {
        let catalog = Catalog::open_in_memory().expect("open");
        // Every optional field is given a distinct value, so an unbound
        // parameter or dropped column fails an assertion.
        let written = NewBookState::new(100_000_001, 1, "embed")
            .embed_model("qwen3")
            .parsed_at("2026-01-01T00:00:00Z")
            .embedded_at("2026-01-02T00:00:00Z")
            .ocr_marker_finished_at("2026-01-01T12:00:00Z")
            .last_error("none");
        catalog.upsert_book_state(&written).expect("write");

        let read = catalog
            .book_state(100_000_001)
            .expect("read")
            .expect("present");
        assert_eq!(read.book_root_id, 100_000_001);
        assert_eq!(read.intake_id, 1);
        assert_eq!(read.current_stage, "embed");
        assert_eq!(read.embed_model.as_deref(), Some("qwen3"));
        assert_eq!(read.parsed_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(read.embedded_at.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(
            read.ocr_marker_finished_at.as_deref(),
            Some("2026-01-01T12:00:00Z")
        );
        assert_eq!(read.last_error.as_deref(), Some("none"));
    }

    #[test]
    fn a_missing_book_state_reads_as_none() {
        let catalog = Catalog::open_in_memory().expect("open");
        assert!(catalog.book_state(404).expect("read").is_none());
    }

    #[test]
    fn count_book_states_by_stage_groups_by_current_stage() {
        let catalog = Catalog::open_in_memory().expect("open");
        catalog
            .upsert_book_state(&NewBookState::new(100_000_001, 1, "extract"))
            .expect("write");
        catalog
            .upsert_book_state(&NewBookState::new(100_000_002, 2, "extract"))
            .expect("write");
        catalog
            .upsert_book_state(&NewBookState::new(100_000_003, 3, "embed"))
            .expect("write");

        assert_eq!(
            catalog
                .count_book_states_by_stage("extract")
                .expect("count extract"),
            2
        );
        assert_eq!(
            catalog
                .count_book_states_by_stage("embed")
                .expect("count embed"),
            1
        );
        assert_eq!(
            catalog
                .count_book_states_by_stage("unknown")
                .expect("count unknown"),
            0
        );
    }

    #[test]
    fn upsert_overwrites_a_books_previous_state() {
        let catalog = Catalog::open_in_memory().expect("open");
        catalog
            .upsert_book_state(&NewBookState::new(100_000_001, 1, "extract"))
            .expect("first write");
        catalog
            .upsert_book_state(&NewBookState::new(100_000_001, 1, "embed").embed_model("qwen3"))
            .expect("second write");

        let read = catalog
            .book_state(100_000_001)
            .expect("read")
            .expect("present");
        assert_eq!(read.current_stage, "embed");
        assert_eq!(read.embed_model.as_deref(), Some("qwen3"));
    }
}
