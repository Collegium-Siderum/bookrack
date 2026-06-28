// SPDX-License-Identifier: Apache-2.0

//! The `book_distill_audit` and `book_distill_stage_report` tables —
//! per-run forensic record of one distill build.
//!
//! The evaluation unit of distill is one reference book's reconstruction
//! from its OCR source. Each `distill build` inserts one header row in
//! [`book_distill_audit`](SPEC) and N rows in
//! [`book_distill_stage_report`](STAGE_SPEC), one per pipeline stage, in
//! the same transaction. The pair is meant to be the consumption source
//! for cross-run rollups and diffs: column choices are stable and additive
//! from this point on; downstream surfaces address rows by `(run_id, ord)`.
//!
//! `gate_status` records the verdict of `distill build`'s retention guard
//! at the time the row was written: `pass` / `fail` / `off` (the
//! `--no-retention-check` opt-out). Failing rows are still written before
//! the build bails, so the operator can read back the run that tripped the
//! guard. `gate_threshold` is the numeric threshold the guard ran with,
//! or NULL when `gate_status='off'`.
//!
//! `profile_ref` is a placeholder column for the distill profile
//! fingerprint a later step will land. The writer leaves it as an empty
//! string until then; the column ships now so its position in the row is
//! fixed before downstream rollups consume the table.

use bookrack_dbkit::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec};
use rusqlite::{OptionalExtension, Row, named_params};

use crate::{Catalog, Result};

/// The single source of truth for the `book_distill_audit` table's
/// schema. Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "book_distill_audit",
    comment: Some("One row per distill build of one reference book."),
    columns: &[
        ColumnSpec::int("run_id").pk_autoinc(),
        ColumnSpec::text("book_slug").not_null(),
        ColumnSpec::text("source_path").not_null(),
        ColumnSpec::text("started_at")
            .not_null()
            .comment("ISO-8601 UTC"),
        ColumnSpec::text("finished_at")
            .not_null()
            .comment("ISO-8601 UTC"),
        ColumnSpec::int("pages").not_null(),
        ColumnSpec::int("blocks").not_null(),
        ColumnSpec::int("raws").not_null(),
        ColumnSpec::int("splits").not_null(),
        ColumnSpec::int("entries").not_null(),
        ColumnSpec::int("unmatched_lines").not_null(),
        ColumnSpec::int("pair_mismatch").not_null(),
        ColumnSpec::text("gate_status")
            .not_null()
            .check("gate_status IN ('pass', 'fail', 'off')"),
        ColumnSpec::real("gate_threshold")
            .comment("the retention threshold the guard ran with; NULL when gate_status='off'"),
        ColumnSpec::text("profile_ref")
            .not_null()
            .default("''")
            .comment("placeholder for the distill profile fingerprint"),
        ColumnSpec::text("extractor_version").not_null(),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[IndexSpec::on(
        "idx_book_distill_audit_slug_time",
        &["book_slug", "started_at"],
    )],
};

/// The single source of truth for the `book_distill_stage_report` table's
/// schema. Its DDL is rendered from this spec.
pub(crate) const STAGE_SPEC: TableSpec = TableSpec {
    name: "book_distill_stage_report",
    comment: Some("Per-stage cardinality for one distill build."),
    columns: &[
        ColumnSpec::int("run_id")
            .not_null()
            .references(ForeignKey::new(
                "book_distill_audit",
                "run_id",
                OnDelete::Cascade,
            )),
        ColumnSpec::int("ord")
            .not_null()
            .comment("stage position within the pipeline; matches Coverage.stage_reports index"),
        ColumnSpec::text("stage_name").not_null(),
        ColumnSpec::text("in_kind").not_null(),
        ColumnSpec::text("out_kind").not_null(),
        ColumnSpec::int("in_len").not_null(),
        ColumnSpec::int("out_len").not_null(),
    ],
    composite_pk: Some(&["run_id", "ord"]),
    uniques: &[],
    table_checks: &[],
    indexes: &[IndexSpec::on(
        "idx_book_distill_stage_report_stage",
        &["stage_name"],
    )],
};

/// Insert one audit header and return the assigned `run_id`.
const INSERT_HEADER_SQL: &str = "INSERT INTO book_distill_audit \
     (book_slug, source_path, started_at, finished_at, \
      pages, blocks, raws, splits, entries, unmatched_lines, pair_mismatch, \
      gate_status, gate_threshold, profile_ref, extractor_version) \
     VALUES (:book_slug, :source_path, :started_at, :finished_at, \
             :pages, :blocks, :raws, :splits, :entries, :unmatched_lines, :pair_mismatch, \
             :gate_status, :gate_threshold, :profile_ref, :extractor_version) \
     RETURNING run_id";

/// Insert one stage-report row.
const INSERT_STAGE_SQL: &str = "INSERT INTO book_distill_stage_report \
     (run_id, ord, stage_name, in_kind, out_kind, in_len, out_len) \
     VALUES (:run_id, :ord, :stage_name, :in_kind, :out_kind, :in_len, :out_len)";

/// `gate_status` value the writer stamps when the retention guard
/// accepted the run.
pub const GATE_STATUS_PASS: &str = "pass";

/// `gate_status` value the writer stamps when the retention guard
/// rejected the run. The row is still written before the build bails.
pub const GATE_STATUS_FAIL: &str = "fail";

/// `gate_status` value the writer stamps when the operator passed
/// `--no-retention-check` and the guard did not run.
pub const GATE_STATUS_OFF: &str = "off";

/// A `SELECT` of every header column with `tail` appended; column list
/// from [`SPEC`].
fn select_header_sql(tail: &str) -> String {
    format!(
        "SELECT {} FROM book_distill_audit {tail}",
        SPEC.select_list()
    )
}

/// A `SELECT` of every stage-report column with `tail` appended; column
/// list from [`STAGE_SPEC`].
fn select_stage_sql(tail: &str) -> String {
    format!(
        "SELECT {} FROM book_distill_stage_report {tail}",
        STAGE_SPEC.select_list()
    )
}

/// One row about to be written to `book_distill_audit`.
#[derive(Debug, Clone)]
pub struct NewBookDistillAudit {
    /// The reference book's slug, matching `reference.books.book_slug`.
    pub book_slug: String,
    /// The distill build entry path, as the operator supplied it.
    pub source_path: String,
    /// When the pipeline started, ISO-8601 UTC.
    pub started_at: String,
    /// When the pipeline returned, ISO-8601 UTC.
    pub finished_at: String,
    /// `Coverage.pages`.
    pub pages: i64,
    /// `Coverage.blocks`.
    pub blocks: i64,
    /// `Coverage.raws`.
    pub raws: i64,
    /// `Coverage.splits`.
    pub splits: i64,
    /// `Coverage.entries`.
    pub entries: i64,
    /// `Coverage.unmatched_lines`.
    pub unmatched_lines: i64,
    /// `Coverage.pair_mismatch`.
    pub pair_mismatch: i64,
    /// `pass` / `fail` / `off` — see [`GATE_STATUS_PASS`], etc.
    pub gate_status: String,
    /// The retention threshold the guard ran with, when it ran.
    pub gate_threshold: Option<f64>,
    /// Placeholder reference to the distill profile fingerprint. The
    /// current writer always passes an empty string.
    pub profile_ref: String,
    /// The version stamp of the book's parser at build time.
    pub extractor_version: String,
}

/// One row about to be written to `book_distill_stage_report`.
#[derive(Debug, Clone)]
pub struct NewStageReport {
    /// The stage's position in the pipeline, matching
    /// `Coverage.stage_reports` index.
    pub ord: i64,
    /// The stage's display name.
    pub stage_name: String,
    /// The stage's input variant name.
    pub in_kind: String,
    /// The stage's output variant name.
    pub out_kind: String,
    /// The stage's input cardinality.
    pub in_len: i64,
    /// The stage's output cardinality.
    pub out_len: i64,
}

/// One `book_distill_audit` row, read back from the database.
#[derive(Debug, Clone, PartialEq)]
pub struct BookDistillAudit {
    /// Surrogate key, assigned by the database.
    pub run_id: i64,
    /// See [`NewBookDistillAudit::book_slug`].
    pub book_slug: String,
    /// See [`NewBookDistillAudit::source_path`].
    pub source_path: String,
    /// See [`NewBookDistillAudit::started_at`].
    pub started_at: String,
    /// See [`NewBookDistillAudit::finished_at`].
    pub finished_at: String,
    /// See [`NewBookDistillAudit::pages`].
    pub pages: i64,
    /// See [`NewBookDistillAudit::blocks`].
    pub blocks: i64,
    /// See [`NewBookDistillAudit::raws`].
    pub raws: i64,
    /// See [`NewBookDistillAudit::splits`].
    pub splits: i64,
    /// See [`NewBookDistillAudit::entries`].
    pub entries: i64,
    /// See [`NewBookDistillAudit::unmatched_lines`].
    pub unmatched_lines: i64,
    /// See [`NewBookDistillAudit::pair_mismatch`].
    pub pair_mismatch: i64,
    /// See [`NewBookDistillAudit::gate_status`].
    pub gate_status: String,
    /// See [`NewBookDistillAudit::gate_threshold`].
    pub gate_threshold: Option<f64>,
    /// See [`NewBookDistillAudit::profile_ref`].
    pub profile_ref: String,
    /// See [`NewBookDistillAudit::extractor_version`].
    pub extractor_version: String,
}

impl BookDistillAudit {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<BookDistillAudit> {
        Ok(BookDistillAudit {
            run_id: row.get("run_id")?,
            book_slug: row.get("book_slug")?,
            source_path: row.get("source_path")?,
            started_at: row.get("started_at")?,
            finished_at: row.get("finished_at")?,
            pages: row.get("pages")?,
            blocks: row.get("blocks")?,
            raws: row.get("raws")?,
            splits: row.get("splits")?,
            entries: row.get("entries")?,
            unmatched_lines: row.get("unmatched_lines")?,
            pair_mismatch: row.get("pair_mismatch")?,
            gate_status: row.get("gate_status")?,
            gate_threshold: row.get("gate_threshold")?,
            profile_ref: row.get("profile_ref")?,
            extractor_version: row.get("extractor_version")?,
        })
    }
}

/// One `book_distill_stage_report` row, read back from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookDistillStageReport {
    /// Foreign key to [`BookDistillAudit::run_id`].
    pub run_id: i64,
    /// Stage position within the pipeline.
    pub ord: i64,
    /// Stage display name.
    pub stage_name: String,
    /// Stage input variant name.
    pub in_kind: String,
    /// Stage output variant name.
    pub out_kind: String,
    /// Stage input cardinality.
    pub in_len: i64,
    /// Stage output cardinality.
    pub out_len: i64,
}

impl BookDistillStageReport {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<BookDistillStageReport> {
        Ok(BookDistillStageReport {
            run_id: row.get("run_id")?,
            ord: row.get("ord")?,
            stage_name: row.get("stage_name")?,
            in_kind: row.get("in_kind")?,
            out_kind: row.get("out_kind")?,
            in_len: row.get("in_len")?,
            out_len: row.get("out_len")?,
        })
    }
}

impl Catalog {
    /// Insert one audit header and its stage-report rows in a single
    /// transaction, returning the assigned `run_id`.
    pub fn insert_distill_audit(
        &mut self,
        header: &NewBookDistillAudit,
        stages: &[NewStageReport],
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let run_id: i64 = tx.query_row(
            INSERT_HEADER_SQL,
            named_params! {
                ":book_slug": header.book_slug,
                ":source_path": header.source_path,
                ":started_at": header.started_at,
                ":finished_at": header.finished_at,
                ":pages": header.pages,
                ":blocks": header.blocks,
                ":raws": header.raws,
                ":splits": header.splits,
                ":entries": header.entries,
                ":unmatched_lines": header.unmatched_lines,
                ":pair_mismatch": header.pair_mismatch,
                ":gate_status": header.gate_status,
                ":gate_threshold": header.gate_threshold,
                ":profile_ref": header.profile_ref,
                ":extractor_version": header.extractor_version,
            },
            |row| row.get(0),
        )?;
        {
            let mut stmt = tx.prepare(INSERT_STAGE_SQL)?;
            for stage in stages {
                stmt.execute(named_params! {
                    ":run_id": run_id,
                    ":ord": stage.ord,
                    ":stage_name": stage.stage_name,
                    ":in_kind": stage.in_kind,
                    ":out_kind": stage.out_kind,
                    ":in_len": stage.in_len,
                    ":out_len": stage.out_len,
                })?;
            }
        }
        tx.commit()?;
        Ok(run_id)
    }

    /// Fetch one distill audit row by id.
    pub fn distill_audit(&self, run_id: i64) -> Result<Option<BookDistillAudit>> {
        let mut stmt = self
            .conn
            .prepare(&select_header_sql("WHERE run_id = :run_id"))?;
        let row = stmt
            .query_row(
                named_params! { ":run_id": run_id },
                BookDistillAudit::from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Every distill audit row for `book_slug`, newest first.
    pub fn distill_audits_for_book(&self, book_slug: &str) -> Result<Vec<BookDistillAudit>> {
        let mut stmt = self.conn.prepare(&select_header_sql(
            "WHERE book_slug = :book_slug ORDER BY started_at DESC, run_id DESC",
        ))?;
        let rows = stmt
            .query_map(
                named_params! { ":book_slug": book_slug },
                BookDistillAudit::from_row,
            )?
            .collect::<rusqlite::Result<Vec<BookDistillAudit>>>()?;
        Ok(rows)
    }

    /// Every stage-report row for one distill run, in pipeline order.
    pub fn distill_stage_reports(&self, run_id: i64) -> Result<Vec<BookDistillStageReport>> {
        let mut stmt = self
            .conn
            .prepare(&select_stage_sql("WHERE run_id = :run_id ORDER BY ord"))?;
        let rows = stmt
            .query_map(
                named_params! { ":run_id": run_id },
                BookDistillStageReport::from_row,
            )?
            .collect::<rusqlite::Result<Vec<BookDistillStageReport>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(slug: &str) -> NewBookDistillAudit {
        NewBookDistillAudit {
            book_slug: slug.to_string(),
            source_path: format!("/data/reference/{slug}/source.md"),
            started_at: "2026-06-28T10:00:00Z".to_string(),
            finished_at: "2026-06-28T10:00:05Z".to_string(),
            pages: 42,
            blocks: 50,
            raws: 100,
            splits: 110,
            entries: 95,
            unmatched_lines: 3,
            pair_mismatch: 1,
            gate_status: GATE_STATUS_PASS.to_string(),
            gate_threshold: Some(0.10),
            profile_ref: String::new(),
            extractor_version: "0.1.0".to_string(),
        }
    }

    fn stage(ord: i64, name: &str) -> NewStageReport {
        NewStageReport {
            ord,
            stage_name: name.to_string(),
            in_kind: "pages".to_string(),
            out_kind: "pages".to_string(),
            in_len: 10,
            out_len: 9,
        }
    }

    #[test]
    fn a_distill_audit_round_trips_every_column() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let run_id = catalog
            .insert_distill_audit(&header("tiny"), &[stage(0, "split"), stage(1, "walk")])
            .expect("insert");
        assert!(run_id > 0);

        let read = catalog
            .distill_audit(run_id)
            .expect("read")
            .expect("present");
        assert_eq!(read.book_slug, "tiny");
        assert_eq!(read.source_path, "/data/reference/tiny/source.md");
        assert_eq!(read.started_at, "2026-06-28T10:00:00Z");
        assert_eq!(read.finished_at, "2026-06-28T10:00:05Z");
        assert_eq!(read.pages, 42);
        assert_eq!(read.entries, 95);
        assert_eq!(read.pair_mismatch, 1);
        assert_eq!(read.gate_status, GATE_STATUS_PASS);
        assert_eq!(read.gate_threshold, Some(0.10));
        assert_eq!(read.profile_ref, "");
        assert_eq!(read.extractor_version, "0.1.0");

        let stages = catalog.distill_stage_reports(run_id).expect("read stages");
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].ord, 0);
        assert_eq!(stages[0].stage_name, "split");
        assert_eq!(stages[1].ord, 1);
        assert_eq!(stages[1].stage_name, "walk");
    }

    #[test]
    fn a_distill_audit_with_no_stages_writes_only_the_header() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let run_id = catalog
            .insert_distill_audit(&header("tiny"), &[])
            .expect("insert");
        assert!(
            catalog
                .distill_stage_reports(run_id)
                .expect("read")
                .is_empty()
        );
        assert!(catalog.distill_audit(run_id).expect("read").is_some());
    }

    #[test]
    fn distill_audits_for_book_orders_newest_first() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let mut h1 = header("tiny");
        h1.started_at = "2026-06-28T09:00:00Z".to_string();
        let mut h2 = header("tiny");
        h2.started_at = "2026-06-28T11:00:00Z".to_string();
        let id1 = catalog.insert_distill_audit(&h1, &[]).expect("insert 1");
        let id2 = catalog.insert_distill_audit(&h2, &[]).expect("insert 2");
        let rows = catalog.distill_audits_for_book("tiny").expect("read");
        let ids: Vec<i64> = rows.iter().map(|r| r.run_id).collect();
        assert_eq!(ids, [id2, id1]);
    }

    #[test]
    fn gate_off_persists_a_null_threshold() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let mut h = header("tiny");
        h.gate_status = GATE_STATUS_OFF.to_string();
        h.gate_threshold = None;
        let run_id = catalog.insert_distill_audit(&h, &[]).expect("insert");
        let read = catalog
            .distill_audit(run_id)
            .expect("read")
            .expect("present");
        assert_eq!(read.gate_status, GATE_STATUS_OFF);
        assert_eq!(read.gate_threshold, None);
    }

    #[test]
    fn gate_status_check_rejects_an_unknown_value() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let mut h = header("tiny");
        h.gate_status = "unknown".to_string();
        let err = catalog
            .insert_distill_audit(&h, &[])
            .expect_err("CHECK must reject");
        assert!(matches!(err, crate::CatalogError::Sqlite(_)), "{err:?}");
    }

    #[test]
    fn deleting_the_header_cascades_to_its_stage_rows() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let run_id = catalog
            .insert_distill_audit(&header("tiny"), &[stage(0, "split")])
            .expect("insert");
        assert_eq!(
            catalog.distill_stage_reports(run_id).expect("read").len(),
            1
        );
        catalog
            .conn
            .execute(
                "DELETE FROM book_distill_audit WHERE run_id = ?1",
                rusqlite::params![run_id],
            )
            .expect("delete header");
        assert!(
            catalog
                .distill_stage_reports(run_id)
                .expect("read")
                .is_empty()
        );
    }
}
