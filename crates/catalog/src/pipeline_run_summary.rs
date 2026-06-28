// SPDX-License-Identifier: Apache-2.0

//! The `pipeline_run_summary` table — one materialized rollup row per
//! `pipeline_runs` row.
//!
//! Computing the rollup at read time would mean scanning every audit
//! table for one `pipeline_run_id` on each open of the runs surface;
//! the writer instead refreshes one row here at run close and the read
//! path serves the cached aggregate. Keyed by `pipeline_run_id` with
//! `ON DELETE CASCADE`, so dropping a run drops its rollup in the same
//! statement.
//!
//! The four JSON columns hold dimension-keyed counters:
//!
//!   * `verdict_counts` — `{ "clean": N, "needs_work": M, ... }`
//!   * `flag_counts` — `{ "flag_doi_invalid_format": K, ... }`
//!   * `coverage_summary` — free-form coverage rollups
//!     (`{ "retention_avg": 0.95, "pair_mismatch_total": 3, ... }`)
//!
//! All three default to `'{}'` so an upsert that has nothing to report
//! still stores a parseable JSON object. The compute helper that runs
//! the four SELECTs and assembles the row lands in a later commit;
//! this commit ships the table, the typed row pair, and the upsert
//! primitive so the writer at run-close can land its rollup.

use bookrack_dbkit::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec};
use rusqlite::{OptionalExtension, Row, named_params};

use crate::{Catalog, Result};

/// The single source of truth for the `pipeline_run_summary` table's
/// schema. Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "pipeline_run_summary",
    comment: Some(
        "Materialized rollup of one pipeline_runs row's audit aggregates; refreshed at run close.",
    ),
    columns: &[
        ColumnSpec::text("pipeline_run_id")
            .primary_key()
            .references(ForeignKey::new(
                "pipeline_runs",
                "pipeline_run_id",
                OnDelete::Cascade,
            )),
        ColumnSpec::int("n_books")
            .not_null()
            .default("0")
            .comment("count of book_distill_audit rows for this run"),
        ColumnSpec::int("n_papers")
            .not_null()
            .default("0")
            .comment("count of node_paper_audit rows for this run"),
        ColumnSpec::text("verdict_counts")
            .not_null()
            .default("'{}'")
            .comment("JSON: verdict -> count"),
        ColumnSpec::text("flag_counts")
            .not_null()
            .default("'{}'")
            .comment("JSON: flag column name -> count"),
        ColumnSpec::text("coverage_summary")
            .not_null()
            .default("'{}'")
            .comment("JSON: coverage metric name -> scalar"),
        ColumnSpec::int("wall_clock_ms").comment("end-to-end run duration in milliseconds"),
        ColumnSpec::text("computed_at")
            .not_null()
            .comment("ISO-8601 UTC"),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[] as &[IndexSpec],
};

/// Upsert one rollup row, overwriting the previous summary for the
/// same `pipeline_run_id`.
const UPSERT_SQL: &str = "INSERT INTO pipeline_run_summary \
     (pipeline_run_id, n_books, n_papers, verdict_counts, flag_counts, \
      coverage_summary, wall_clock_ms, computed_at) \
     VALUES (:pipeline_run_id, :n_books, :n_papers, :verdict_counts, :flag_counts, \
             :coverage_summary, :wall_clock_ms, :computed_at) \
     ON CONFLICT(pipeline_run_id) DO UPDATE SET \
       n_books = excluded.n_books, \
       n_papers = excluded.n_papers, \
       verdict_counts = excluded.verdict_counts, \
       flag_counts = excluded.flag_counts, \
       coverage_summary = excluded.coverage_summary, \
       wall_clock_ms = excluded.wall_clock_ms, \
       computed_at = excluded.computed_at";

/// A `SELECT` of every column with `tail` appended; column list from [`SPEC`].
fn select_sql(tail: &str) -> String {
    format!(
        "SELECT {} FROM pipeline_run_summary {tail}",
        SPEC.select_list()
    )
}

/// One row about to be written to `pipeline_run_summary`.
#[derive(Debug, Clone)]
pub struct NewPipelineRunSummary {
    /// Foreign key to [`crate::PipelineRun::pipeline_run_id`].
    pub pipeline_run_id: String,
    /// Count of `book_distill_audit` rows tagged with this run.
    pub n_books: i64,
    /// Count of `node_paper_audit` rows tagged with this run.
    pub n_papers: i64,
    /// JSON: verdict label -> count.
    pub verdict_counts: String,
    /// JSON: flag column name -> count.
    pub flag_counts: String,
    /// JSON: coverage metric name -> scalar.
    pub coverage_summary: String,
    /// End-to-end run duration in milliseconds.
    pub wall_clock_ms: Option<i64>,
    /// When the summary was computed, ISO-8601 UTC.
    pub computed_at: String,
}

/// One `pipeline_run_summary` row, read back from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineRunSummary {
    /// See [`NewPipelineRunSummary::pipeline_run_id`].
    pub pipeline_run_id: String,
    /// See [`NewPipelineRunSummary::n_books`].
    pub n_books: i64,
    /// See [`NewPipelineRunSummary::n_papers`].
    pub n_papers: i64,
    /// See [`NewPipelineRunSummary::verdict_counts`].
    pub verdict_counts: String,
    /// See [`NewPipelineRunSummary::flag_counts`].
    pub flag_counts: String,
    /// See [`NewPipelineRunSummary::coverage_summary`].
    pub coverage_summary: String,
    /// See [`NewPipelineRunSummary::wall_clock_ms`].
    pub wall_clock_ms: Option<i64>,
    /// See [`NewPipelineRunSummary::computed_at`].
    pub computed_at: String,
}

impl PipelineRunSummary {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<PipelineRunSummary> {
        Ok(PipelineRunSummary {
            pipeline_run_id: row.get("pipeline_run_id")?,
            n_books: row.get("n_books")?,
            n_papers: row.get("n_papers")?,
            verdict_counts: row.get("verdict_counts")?,
            flag_counts: row.get("flag_counts")?,
            coverage_summary: row.get("coverage_summary")?,
            wall_clock_ms: row.get("wall_clock_ms")?,
            computed_at: row.get("computed_at")?,
        })
    }
}

impl Catalog {
    /// Upsert one `pipeline_run_summary` row, overwriting any previous
    /// rollup for the same `pipeline_run_id`. The compute helper that
    /// fills the JSON columns from audit-table aggregates builds on
    /// this primitive.
    pub fn upsert_pipeline_run_summary(&self, row: &NewPipelineRunSummary) -> Result<()> {
        self.conn.execute(
            UPSERT_SQL,
            named_params! {
                ":pipeline_run_id": row.pipeline_run_id,
                ":n_books": row.n_books,
                ":n_papers": row.n_papers,
                ":verdict_counts": row.verdict_counts,
                ":flag_counts": row.flag_counts,
                ":coverage_summary": row.coverage_summary,
                ":wall_clock_ms": row.wall_clock_ms,
                ":computed_at": row.computed_at,
            },
        )?;
        Ok(())
    }

    /// Fetch one `pipeline_run_summary` row by id, or `None` if no
    /// rollup has been computed for that run.
    pub fn pipeline_run_summary(
        &self,
        pipeline_run_id: &str,
    ) -> Result<Option<PipelineRunSummary>> {
        let mut stmt = self
            .conn
            .prepare(&select_sql("WHERE pipeline_run_id = :pipeline_run_id"))?;
        let row = stmt
            .query_row(
                named_params! { ":pipeline_run_id": pipeline_run_id },
                PipelineRunSummary::from_row,
            )
            .optional()?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewPipelineRun;

    fn seed_parent_run(catalog: &Catalog, pipeline_run_id: &str) {
        catalog
            .insert_pipeline_run(&NewPipelineRun {
                pipeline_run_id: pipeline_run_id.to_string(),
                command: "distill_build".to_string(),
                command_args: None,
                library_root: None,
                started_at: "2026-06-28T10:00:00Z".to_string(),
                finished_at: None,
                status: Some("running".to_string()),
            })
            .expect("seed parent");
    }

    fn fixture(pipeline_run_id: &str) -> NewPipelineRunSummary {
        NewPipelineRunSummary {
            pipeline_run_id: pipeline_run_id.to_string(),
            n_books: 3,
            n_papers: 7,
            verdict_counts: r#"{"clean":3,"needs_work":1}"#.to_string(),
            flag_counts: r#"{"flag_doi_invalid_format":2}"#.to_string(),
            coverage_summary: r#"{"retention_avg":0.95}"#.to_string(),
            wall_clock_ms: Some(12_500),
            computed_at: "2026-06-28T10:00:05Z".to_string(),
        }
    }

    #[test]
    fn pipeline_run_summary_upsert_round_trip() {
        let catalog = Catalog::open_in_memory().expect("open");
        let run_id = "distill_build-2026-06-28T10:00:00Z-deadbeef";
        seed_parent_run(&catalog, run_id);

        catalog
            .upsert_pipeline_run_summary(&fixture(run_id))
            .expect("upsert");
        let read = catalog
            .pipeline_run_summary(run_id)
            .expect("read")
            .expect("present");
        assert_eq!(read.n_books, 3);
        assert_eq!(read.n_papers, 7);
        assert_eq!(read.verdict_counts, r#"{"clean":3,"needs_work":1}"#);
        assert_eq!(read.flag_counts, r#"{"flag_doi_invalid_format":2}"#);
        assert_eq!(read.coverage_summary, r#"{"retention_avg":0.95}"#);
        assert_eq!(read.wall_clock_ms, Some(12_500));
        assert_eq!(read.computed_at, "2026-06-28T10:00:05Z");
    }

    #[test]
    fn pipeline_run_summary_upsert_overwrites_previous() {
        let catalog = Catalog::open_in_memory().expect("open");
        let run_id = "distill_build-2026-06-28T10:00:00Z-deadbeef";
        seed_parent_run(&catalog, run_id);

        catalog
            .upsert_pipeline_run_summary(&fixture(run_id))
            .expect("first upsert");

        let mut second = fixture(run_id);
        second.n_books = 5;
        second.n_papers = 11;
        second.verdict_counts = r#"{"clean":5}"#.to_string();
        second.flag_counts = "{}".to_string();
        second.coverage_summary = r#"{"retention_avg":0.98}"#.to_string();
        second.wall_clock_ms = Some(14_000);
        second.computed_at = "2026-06-28T10:01:00Z".to_string();

        catalog
            .upsert_pipeline_run_summary(&second)
            .expect("second upsert");

        let read = catalog
            .pipeline_run_summary(run_id)
            .expect("read")
            .expect("present");
        assert_eq!(read.n_books, 5);
        assert_eq!(read.n_papers, 11);
        assert_eq!(read.verdict_counts, r#"{"clean":5}"#);
        assert_eq!(read.flag_counts, "{}");
        assert_eq!(read.coverage_summary, r#"{"retention_avg":0.98}"#);
        assert_eq!(read.wall_clock_ms, Some(14_000));
        assert_eq!(read.computed_at, "2026-06-28T10:01:00Z");
    }
}
