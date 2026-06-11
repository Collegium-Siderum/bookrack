// SPDX-License-Identifier: Apache-2.0

//! The `book_pipeline_audit` table — the pipeline-step log.
//!
//! One append-only row per pipeline sub-step: its outcome, timing, any
//! error, and the run it belongs to. Rows outlive the books they
//! describe — `book_root_id` is a bare soft reference, and the
//! whole-file `source_sha256` is denormalized onto the row so it stays
//! meaningful after the book is deleted.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec, decode};
use rusqlite::{Row, named_params};

use crate::{ActorKind, Catalog, Result};

/// The single source of truth for the `item_pipeline_audit` table's
/// schema. Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "item_pipeline_audit",
    comment: Some(
        "The pipeline-step log, shared between book ingest and paper glean. Audit rows outlive the items they describe.",
    ),
    columns: &[
        ColumnSpec::int("audit_id").pk_autoinc(),
        ColumnSpec::int("book_root_id").comment("soft reference; NULL allowed"),
        ColumnSpec::text("source_sha256").comment("denormalized so the row survives book deletion"),
        ColumnSpec::text("stage").not_null(),
        ColumnSpec::text("sub_step").not_null(),
        ColumnSpec::text("outcome")
            .not_null()
            .comment("ok / fail / partial / skipped"),
        ColumnSpec::text("adapter"),
        ColumnSpec::text("metric_summary").comment("JSON"),
        ColumnSpec::text("error_message"),
        ColumnSpec::int("duration_ms"),
        ColumnSpec::text("ts").not_null(),
        ColumnSpec::text("pipeline_run_id")
            .not_null()
            .comment("ties one pipeline run together"),
        ColumnSpec::text("actor_kind")
            .not_null()
            .check("actor_kind IN ('human', 'llm', 'import', 'pipeline', 'system')"),
        ColumnSpec::text("actor_detail").comment("model name, import source, run id, ..."),
        ColumnSpec::text("session_id"),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[
        IndexSpec::on("idx_pa_book", &["book_root_id", "ts"]),
        IndexSpec::on("idx_pa_run", &["pipeline_run_id", "ts"]),
        IndexSpec::on("idx_pa_stage", &["stage", "ts"]),
        IndexSpec::on("idx_pa_outcome", &["outcome", "ts"]),
    ],
};

/// Insert one audit row and return its surrogate id. `ts` is generated
/// by SQLite so the whole crate shares one timestamp source.
const INSERT_SQL: &str = "INSERT INTO item_pipeline_audit \
     (book_root_id, source_sha256, stage, sub_step, outcome, adapter, \
      metric_summary, error_message, duration_ms, ts, pipeline_run_id, \
      actor_kind, actor_detail, session_id) \
     VALUES (:book_root_id, :source_sha256, :stage, :sub_step, :outcome, \
             :adapter, :metric_summary, :error_message, :duration_ms, \
             strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), :pipeline_run_id, \
             :actor_kind, :actor_detail, :session_id) \
     RETURNING audit_id";

/// A `SELECT` of every column with `tail` appended; column list from
/// [`SPEC`].
fn select_sql(tail: &str) -> String {
    format!(
        "SELECT {} FROM item_pipeline_audit {tail}",
        SPEC.select_list()
    )
}

/// One `book_pipeline_audit` row — a single recorded pipeline sub-step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookPipelineAudit {
    /// Surrogate key, assigned by the database.
    pub audit_id: i64,
    /// The book's root node id — a soft reference; `None` when unknown.
    pub book_root_id: Option<i64>,
    /// The whole-file hash, denormalized so the row survives deletion.
    pub source_sha256: Option<String>,
    /// The pipeline stage.
    pub stage: String,
    /// The sub-step within the stage.
    pub sub_step: String,
    /// How the sub-step ended (`ok` / `fail` / `partial` / `skipped`).
    pub outcome: String,
    /// The adapter that ran the sub-step, when one applies.
    pub adapter: Option<String>,
    /// A JSON summary of metrics from the sub-step.
    pub metric_summary: Option<String>,
    /// The error text, when the sub-step failed.
    pub error_message: Option<String>,
    /// How long the sub-step took, in milliseconds.
    pub duration_ms: Option<i64>,
    /// When the sub-step ran, ISO-8601 UTC.
    pub ts: String,
    /// The run id tying every sub-step of one pipeline run together.
    pub pipeline_run_id: String,
    /// What kind of actor drove the run.
    pub actor_kind: ActorKind,
    /// The variable part of the actor's identity.
    pub actor_detail: Option<String>,
    /// The session the run belongs to.
    pub session_id: Option<String>,
}

impl BookPipelineAudit {
    /// Build a [`BookPipelineAudit`] from a row including every column.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<BookPipelineAudit> {
        Ok(BookPipelineAudit {
            audit_id: row.get("audit_id")?,
            book_root_id: row.get("book_root_id")?,
            source_sha256: row.get("source_sha256")?,
            stage: row.get("stage")?,
            sub_step: row.get("sub_step")?,
            outcome: row.get("outcome")?,
            adapter: row.get("adapter")?,
            metric_summary: row.get("metric_summary")?,
            error_message: row.get("error_message")?,
            duration_ms: row.get("duration_ms")?,
            ts: row.get("ts")?,
            pipeline_run_id: row.get("pipeline_run_id")?,
            actor_kind: decode(row, "actor_kind", ActorKind::from_db_str)?,
            actor_detail: row.get("actor_detail")?,
            session_id: row.get("session_id")?,
        })
    }
}

/// An audit row about to be written. The surrogate `audit_id` and the
/// `ts` timestamp are assigned by the database. The row is a flat record
/// written as a unit, so its fields are public rather than builder
/// methods; [`NewBookPipelineAudit::new`] sets the mandatory ones.
#[derive(Debug, Clone)]
pub struct NewBookPipelineAudit {
    /// The book's root node id, when known.
    pub book_root_id: Option<i64>,
    /// The whole-file hash, denormalized onto the row.
    pub source_sha256: Option<String>,
    /// The pipeline stage.
    pub stage: String,
    /// The sub-step within the stage.
    pub sub_step: String,
    /// How the sub-step ended.
    pub outcome: String,
    /// The adapter that ran the sub-step.
    pub adapter: Option<String>,
    /// A JSON summary of metrics.
    pub metric_summary: Option<String>,
    /// The error text, when the sub-step failed.
    pub error_message: Option<String>,
    /// How long the sub-step took, in milliseconds.
    pub duration_ms: Option<i64>,
    /// The run id tying one pipeline run together.
    pub pipeline_run_id: String,
    /// What kind of actor drove the run.
    pub actor_kind: ActorKind,
    /// The variable part of the actor's identity.
    pub actor_detail: Option<String>,
    /// The session the run belongs to.
    pub session_id: Option<String>,
}

impl NewBookPipelineAudit {
    /// An audit row for one sub-step, with every optional field cleared.
    pub fn new(
        stage: impl Into<String>,
        sub_step: impl Into<String>,
        outcome: impl Into<String>,
        pipeline_run_id: impl Into<String>,
        actor_kind: ActorKind,
    ) -> NewBookPipelineAudit {
        NewBookPipelineAudit {
            book_root_id: None,
            source_sha256: None,
            stage: stage.into(),
            sub_step: sub_step.into(),
            outcome: outcome.into(),
            adapter: None,
            metric_summary: None,
            error_message: None,
            duration_ms: None,
            pipeline_run_id: pipeline_run_id.into(),
            actor_kind,
            actor_detail: None,
            session_id: None,
        }
    }
}

impl Catalog {
    /// Append one row to the pipeline-step log, returning its assigned
    /// `audit_id`.
    pub fn record_pipeline_audit(&self, new: &NewBookPipelineAudit) -> Result<i64> {
        let id = self.conn.query_row(
            INSERT_SQL,
            named_params! {
                ":book_root_id": new.book_root_id,
                ":source_sha256": new.source_sha256,
                ":stage": new.stage,
                ":sub_step": new.sub_step,
                ":outcome": new.outcome,
                ":adapter": new.adapter,
                ":metric_summary": new.metric_summary,
                ":error_message": new.error_message,
                ":duration_ms": new.duration_ms,
                ":pipeline_run_id": new.pipeline_run_id,
                ":actor_kind": new.actor_kind.as_str(),
                ":actor_detail": new.actor_detail,
                ":session_id": new.session_id,
            },
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Every sub-step of one pipeline run, oldest first.
    pub fn pipeline_audit_for_run(&self, pipeline_run_id: &str) -> Result<Vec<BookPipelineAudit>> {
        let mut stmt = self.conn.prepare(&select_sql(
            "WHERE pipeline_run_id = :pipeline_run_id ORDER BY ts, audit_id",
        ))?;
        let rows = stmt
            .query_map(
                named_params! { ":pipeline_run_id": pipeline_run_id },
                BookPipelineAudit::from_row,
            )?
            .collect::<rusqlite::Result<Vec<BookPipelineAudit>>>()?;
        Ok(rows)
    }

    /// Every recorded sub-step for one book, oldest first. Rows logged
    /// before the book's root id was known (e.g. an extract that failed
    /// before structure) carry a `NULL` `book_root_id` and are not
    /// returned here; query by run id to see those.
    pub fn pipeline_audit_for_book(&self, book_root_id: i64) -> Result<Vec<BookPipelineAudit>> {
        let mut stmt = self.conn.prepare(&select_sql(
            "WHERE book_root_id = :book_root_id ORDER BY ts, audit_id",
        ))?;
        let rows = stmt
            .query_map(
                named_params! { ":book_root_id": book_root_id },
                BookPipelineAudit::from_row,
            )?
            .collect::<rusqlite::Result<Vec<BookPipelineAudit>>>()?;
        Ok(rows)
    }

    /// Pipeline-audit rows with `ts >= since_ts`, newest first, capped at
    /// `limit` rows. The forensic time window the diagnose collector
    /// consumes.
    pub fn recent_pipeline_audit(
        &self,
        since_ts: &str,
        limit: u32,
    ) -> Result<Vec<BookPipelineAudit>> {
        let mut stmt = self.conn.prepare(&select_sql(
            "WHERE ts >= :since_ts ORDER BY ts DESC, audit_id DESC LIMIT :limit",
        ))?;
        let rows = stmt
            .query_map(
                named_params! { ":since_ts": since_ts, ":limit": limit },
                BookPipelineAudit::from_row,
            )?
            .collect::<rusqlite::Result<Vec<BookPipelineAudit>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `NewBookPipelineAudit` with every optional field set, so a
    /// dropped column or unbound parameter fails an assertion.
    fn fully_populated() -> NewBookPipelineAudit {
        let mut audit =
            NewBookPipelineAudit::new("structure", "parse_toc", "ok", "run-1", ActorKind::Pipeline);
        audit.book_root_id = Some(100_000_001);
        audit.source_sha256 = Some("sha-abc".into());
        audit.adapter = Some("epub".into());
        audit.metric_summary = Some(r#"{"nodes":42}"#.into());
        audit.error_message = Some("none".into());
        audit.duration_ms = Some(1234);
        audit.actor_detail = Some("ingest".into());
        audit.session_id = Some("sess-1".into());
        audit
    }

    #[test]
    fn a_pipeline_audit_row_round_trips_every_field() {
        let catalog = Catalog::open_in_memory().expect("open");
        let id = catalog
            .record_pipeline_audit(&fully_populated())
            .expect("record");
        assert!(id > 0);

        let all = catalog.pipeline_audit_for_run("run-1").expect("read");
        assert_eq!(all.len(), 1);
        let row = &all[0];
        assert_eq!(row.audit_id, id);
        assert_eq!(row.book_root_id, Some(100_000_001));
        assert_eq!(row.source_sha256.as_deref(), Some("sha-abc"));
        assert_eq!(row.stage, "structure");
        assert_eq!(row.sub_step, "parse_toc");
        assert_eq!(row.outcome, "ok");
        assert_eq!(row.adapter.as_deref(), Some("epub"));
        assert_eq!(row.metric_summary.as_deref(), Some(r#"{"nodes":42}"#));
        assert_eq!(row.error_message.as_deref(), Some("none"));
        assert_eq!(row.duration_ms, Some(1234));
        assert!(!row.ts.is_empty());
        assert_eq!(row.pipeline_run_id, "run-1");
        assert_eq!(row.actor_kind, ActorKind::Pipeline);
        assert_eq!(row.actor_detail.as_deref(), Some("ingest"));
        assert_eq!(row.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn a_run_groups_its_sub_steps_in_insertion_order() {
        let catalog = Catalog::open_in_memory().expect("open");
        for sub_step in ["extract", "normalize", "embed"] {
            catalog
                .record_pipeline_audit(&NewBookPipelineAudit::new(
                    "ingest",
                    sub_step,
                    "ok",
                    "run-7",
                    ActorKind::Pipeline,
                ))
                .expect("record");
        }
        // A sub-step of a different run must not leak into the result.
        catalog
            .record_pipeline_audit(&NewBookPipelineAudit::new(
                "ingest",
                "extract",
                "ok",
                "run-other",
                ActorKind::Pipeline,
            ))
            .expect("record");

        let sub_steps: Vec<String> = catalog
            .pipeline_audit_for_run("run-7")
            .expect("read")
            .into_iter()
            .map(|row| row.sub_step)
            .collect();
        assert_eq!(sub_steps, ["extract", "normalize", "embed"]);
    }

    #[test]
    fn an_unknown_run_reads_empty() {
        let catalog = Catalog::open_in_memory().expect("open");
        assert!(
            catalog
                .pipeline_audit_for_run("run-404")
                .expect("read")
                .is_empty()
        );
    }

    /// Insert a row, then back-date its `ts` so a cutoff filter has
    /// something deterministic to compare against. The default `ts` is
    /// `now`, which would defeat a `since_ts` filter in a unit test.
    fn record_at(catalog: &Catalog, ts: &str, sub_step: &str) -> i64 {
        let id = catalog
            .record_pipeline_audit(&NewBookPipelineAudit::new(
                "structure",
                sub_step,
                "ok",
                "run-1",
                ActorKind::Pipeline,
            ))
            .expect("record");
        catalog
            .conn
            .execute(
                "UPDATE item_pipeline_audit SET ts = ?1 WHERE audit_id = ?2",
                rusqlite::params![ts, id],
            )
            .expect("backdate ts");
        id
    }

    #[test]
    fn recent_pipeline_audit_returns_rows_since_cutoff_newest_first() {
        let catalog = Catalog::open_in_memory().expect("open");
        record_at(&catalog, "2026-06-01T00:00:00Z", "step_a");
        record_at(&catalog, "2026-06-02T00:00:00Z", "step_b");
        record_at(&catalog, "2026-06-03T00:00:00Z", "step_c");

        let rows = catalog
            .recent_pipeline_audit("2026-06-02T00:00:00Z", 10)
            .expect("read");
        let steps: Vec<&str> = rows.iter().map(|r| r.sub_step.as_str()).collect();
        assert_eq!(steps, ["step_c", "step_b"]);
    }

    #[test]
    fn audit_for_book_returns_only_rows_with_that_root() {
        let catalog = Catalog::open_in_memory().expect("open");
        // A row before the root id is known carries a NULL book_root_id.
        catalog
            .record_pipeline_audit(&NewBookPipelineAudit::new(
                "extract",
                "extract",
                "ok",
                "run-1",
                ActorKind::Pipeline,
            ))
            .expect("record");
        for stage in ["structure", "embed"] {
            let mut row =
                NewBookPipelineAudit::new(stage, stage, "ok", "run-1", ActorKind::Pipeline);
            row.book_root_id = Some(100_000_001);
            catalog.record_pipeline_audit(&row).expect("record");
        }

        let stages: Vec<String> = catalog
            .pipeline_audit_for_book(100_000_001)
            .expect("read")
            .into_iter()
            .map(|row| row.stage)
            .collect();
        // The NULL-root extract row is excluded; the two rooted rows remain.
        assert_eq!(stages, ["structure", "embed"]);
    }
}
