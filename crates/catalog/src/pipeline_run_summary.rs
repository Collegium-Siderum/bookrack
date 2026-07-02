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
//! still stores a parseable JSON object. [`Catalog::compute_run_summary`]
//! reads `book_distill_audit` and `node_paper_audit` for one run and
//! upserts the assembled row; the four aggregates use an explicit
//! `WHERE pipeline_run_id = :pipeline_run_id` so historical rows with
//! NULL `pipeline_run_id` stay out of any single run's rollup.

use bookrack_dbkit::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec};
use rusqlite::{OptionalExtension, Row, named_params};
use serde_json::{Map, Value};

use crate::{Catalog, FLAG_COLUMNS, Result};

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

    /// Materialize the rollup for one run by aggregating its audit rows
    /// and upserting the result. The four SELECTs all key on
    /// `pipeline_run_id = :pipeline_run_id` so historical rows with NULL
    /// `pipeline_run_id` stay outside every single-run rollup. The
    /// returned row is the same one now persisted on
    /// `pipeline_run_summary`.
    pub fn compute_run_summary(&self, pipeline_run_id: &str) -> Result<PipelineRunSummary> {
        let n_books: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM book_distill_audit \
             WHERE pipeline_run_id = :pipeline_run_id",
            named_params! { ":pipeline_run_id": pipeline_run_id },
            |row| row.get(0),
        )?;
        let n_papers: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM node_paper_audit \
             WHERE pipeline_run_id = :pipeline_run_id",
            named_params! { ":pipeline_run_id": pipeline_run_id },
            |row| row.get(0),
        )?;

        let mut verdict_stmt = self.conn.prepare(
            "SELECT verdict, COUNT(*) FROM node_paper_audit \
             WHERE pipeline_run_id = :pipeline_run_id \
             GROUP BY verdict ORDER BY verdict",
        )?;
        let mut verdict_map: Map<String, Value> = Map::new();
        let verdict_rows = verdict_stmt.query_map(
            named_params! { ":pipeline_run_id": pipeline_run_id },
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        for r in verdict_rows {
            let (verdict, count) = r?;
            verdict_map.insert(verdict, Value::from(count));
        }
        let verdict_counts = Value::Object(verdict_map).to_string();

        let flag_select = FLAG_COLUMNS
            .iter()
            .map(|c| format!("COALESCE(SUM({c}), 0) AS {c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let flag_sql = format!(
            "SELECT {flag_select} FROM node_paper_audit \
             WHERE pipeline_run_id = :pipeline_run_id"
        );
        let mut flag_stmt = self.conn.prepare(&flag_sql)?;
        let flag_map = flag_stmt.query_row(
            named_params! { ":pipeline_run_id": pipeline_run_id },
            |row| {
                let mut m: Map<String, Value> = Map::new();
                for (i, col) in FLAG_COLUMNS.iter().enumerate() {
                    let v: i64 = row.get(i)?;
                    if v > 0 {
                        m.insert((*col).to_string(), Value::from(v));
                    }
                }
                Ok(m)
            },
        )?;
        let flag_counts = Value::Object(flag_map).to_string();

        let (pair_mismatch_total, unmatched_lines_total, pages_total, gate_fail_count): (
            i64,
            i64,
            i64,
            i64,
        ) = self.conn.query_row(
            "SELECT COALESCE(SUM(pair_mismatch), 0), \
                    COALESCE(SUM(unmatched_lines), 0), \
                    COALESCE(SUM(pages), 0), \
                    COALESCE(SUM(CASE WHEN gate_status = 'fail' THEN 1 ELSE 0 END), 0) \
             FROM book_distill_audit \
             WHERE pipeline_run_id = :pipeline_run_id",
            named_params! { ":pipeline_run_id": pipeline_run_id },
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let mut coverage_map: Map<String, Value> = Map::new();
        coverage_map.insert(
            "pair_mismatch_total".to_string(),
            Value::from(pair_mismatch_total),
        );
        coverage_map.insert(
            "unmatched_lines_total".to_string(),
            Value::from(unmatched_lines_total),
        );
        coverage_map.insert("pages_total".to_string(), Value::from(pages_total));
        coverage_map.insert("gate_fail_count".to_string(), Value::from(gate_fail_count));
        let coverage_summary = Value::Object(coverage_map).to_string();

        // Wall-clock from the parent run when both timestamps are set;
        // None during an open run (close_pipeline_run has not stamped
        // finished_at yet) or when no matching parent row exists.
        let wall_clock_ms: Option<i64> = self
            .conn
            .query_row(
                "SELECT CAST((julianday(finished_at) - julianday(started_at)) * 86400000.0 AS INTEGER) \
                 FROM pipeline_runs WHERE pipeline_run_id = :pipeline_run_id",
                named_params! { ":pipeline_run_id": pipeline_run_id },
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();

        let computed_at: String =
            self.conn
                .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |row| {
                    row.get(0)
                })?;

        let row = NewPipelineRunSummary {
            pipeline_run_id: pipeline_run_id.to_string(),
            n_books,
            n_papers,
            verdict_counts,
            flag_counts,
            coverage_summary,
            wall_clock_ms,
            computed_at,
        };
        self.upsert_pipeline_run_summary(&row)?;
        Ok(PipelineRunSummary {
            pipeline_run_id: row.pipeline_run_id,
            n_books: row.n_books,
            n_papers: row.n_papers,
            verdict_counts: row.verdict_counts,
            flag_counts: row.flag_counts,
            coverage_summary: row.coverage_summary,
            wall_clock_ms: row.wall_clock_ms,
            computed_at: row.computed_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book_distill_audit::{GATE_STATUS_FAIL, GATE_STATUS_PASS, NewBookDistillAudit};
    use crate::node_paper_audit::{GRADE_COLUMNS, NewNodePaperAudit};
    use crate::{FLAG_COLUMNS, NewPipelineRun};

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

    fn distill_header(slug: &str, run_id: Option<&str>, pair_mismatch: i64) -> NewBookDistillAudit {
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
            pair_mismatch,
            gate_status: GATE_STATUS_PASS.to_string(),
            gate_threshold: Some(0.10),
            profile_ref: String::new(),
            extractor_version: "0.1.0".to_string(),
            pipeline_run_id: run_id.map(str::to_string),
            profile_toggle_summary: None,
        }
    }

    fn paper_audit(
        intake_id: i64,
        verdict: &str,
        run_id: Option<&str>,
        flag_col: Option<&str>,
    ) -> NewNodePaperAudit {
        let mut grades: [String; GRADE_COLUMNS.len()] = Default::default();
        for g in grades.iter_mut() {
            *g = "medium".to_string();
        }
        let mut flags: [u8; FLAG_COLUMNS.len()] = [0; FLAG_COLUMNS.len()];
        if let Some(flag) = flag_col {
            let idx = FLAG_COLUMNS
                .iter()
                .position(|c| *c == flag)
                .expect("known flag column");
            flags[idx] = 1;
        }
        NewNodePaperAudit {
            intake_id,
            scope: "paper".to_string(),
            profile_name: "default".to_string(),
            verdict: verdict.to_string(),
            confidence: "medium".to_string(),
            csl_type: Some("article-journal".to_string()),
            audited_at: "2026-06-28T10:00:00Z".to_string(),
            extractor_version: "0.0.0-test".to_string(),
            grades,
            flags,
            pipeline_run_id: run_id.map(str::to_string),
            profile_fingerprint: None,
            profile_toggle_summary: None,
        }
    }

    #[test]
    fn compute_run_summary_aggregates_two_distill_audits_under_one_run_id() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let run_id = catalog
            .open_pipeline_run("distill_build", None, Some("lib-a"))
            .expect("open run");

        // Two distill audits inside the run, one outside under a
        // sibling run, and one historical row whose pipeline_run_id is
        // NULL. The historical row must stay out of the rollup.
        catalog
            .insert_distill_audit(&distill_header("alpha", Some(&run_id), 1), &[])
            .expect("alpha");
        catalog
            .insert_distill_audit(&distill_header("beta", Some(&run_id), 2), &[])
            .expect("beta");
        catalog
            .insert_pipeline_run(&NewPipelineRun {
                pipeline_run_id: "sibling-run".to_string(),
                command: "distill_build".to_string(),
                command_args: None,
                library_root: None,
                started_at: "2026-06-28T11:00:00Z".to_string(),
                finished_at: None,
                status: Some("running".to_string()),
            })
            .expect("seed sibling");
        catalog
            .insert_distill_audit(&distill_header("gamma", Some("sibling-run"), 99), &[])
            .expect("gamma");
        catalog
            .insert_distill_audit(&distill_header("delta", None, 99), &[])
            .expect("delta historical");

        // Two paper audits inside the run with different verdicts and a
        // flag bit set on one of them.
        catalog
            .upsert_node_paper_audit(&paper_audit(
                1,
                "clean",
                Some(&run_id),
                Some("flag_doi_invalid_format"),
            ))
            .expect("paper clean");
        catalog
            .upsert_node_paper_audit(&paper_audit(2, "needs_work", Some(&run_id), None))
            .expect("paper needs_work");
        catalog
            .upsert_node_paper_audit(&paper_audit(3, "clean", None, None))
            .expect("paper historical");

        catalog.close_pipeline_run(&run_id, "ok").expect("close");

        let summary = catalog.compute_run_summary(&run_id).expect("compute");
        assert_eq!(summary.n_books, 2, "only alpha + beta belong to this run");
        assert_eq!(summary.n_papers, 2, "historical paper row is excluded");

        let verdicts: Value =
            serde_json::from_str(&summary.verdict_counts).expect("verdict_counts json");
        assert_eq!(verdicts["clean"], Value::from(1));
        assert_eq!(verdicts["needs_work"], Value::from(1));

        let flags: Value = serde_json::from_str(&summary.flag_counts).expect("flag_counts json");
        assert_eq!(flags["flag_doi_invalid_format"], Value::from(1));
        // Columns with zero hits drop out of the map; verify by absence.
        assert!(flags.get("flag_empty").is_none());

        let coverage: Value =
            serde_json::from_str(&summary.coverage_summary).expect("coverage_summary json");
        assert_eq!(coverage["pair_mismatch_total"], Value::from(3));
        assert_eq!(coverage["pages_total"], Value::from(84));
        assert_eq!(coverage["unmatched_lines_total"], Value::from(6));
        assert_eq!(coverage["gate_fail_count"], Value::from(0));

        // The rollup landed on the table for read-back.
        let read = catalog
            .pipeline_run_summary(&run_id)
            .expect("read")
            .expect("present");
        assert_eq!(read.n_books, 2);
        assert_eq!(read.n_papers, 2);
    }

    #[test]
    fn compute_run_summary_returns_zeros_for_a_run_with_no_audits() {
        let catalog = Catalog::open_in_memory().expect("open");
        let run_id = catalog
            .open_pipeline_run("dryrun", None, None)
            .expect("open run");
        let summary = catalog.compute_run_summary(&run_id).expect("compute");
        assert_eq!(summary.n_books, 0);
        assert_eq!(summary.n_papers, 0);
        assert_eq!(summary.verdict_counts, "{}");
        assert_eq!(summary.flag_counts, "{}");
        let coverage: Value =
            serde_json::from_str(&summary.coverage_summary).expect("coverage json");
        assert_eq!(coverage["pair_mismatch_total"], Value::from(0));
        assert_eq!(coverage["gate_fail_count"], Value::from(0));
    }

    #[test]
    fn compute_run_summary_counts_gate_failures_in_coverage() {
        let mut catalog = Catalog::open_in_memory().expect("open");
        let run_id = catalog
            .open_pipeline_run("distill_build", None, Some("lib-a"))
            .expect("open run");
        let mut h = distill_header("alpha", Some(&run_id), 0);
        h.gate_status = GATE_STATUS_FAIL.to_string();
        catalog.insert_distill_audit(&h, &[]).expect("insert");
        let summary = catalog.compute_run_summary(&run_id).expect("compute");
        let coverage: Value =
            serde_json::from_str(&summary.coverage_summary).expect("coverage json");
        assert_eq!(coverage["gate_fail_count"], Value::from(1));
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
