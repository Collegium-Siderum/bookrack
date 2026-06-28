// SPDX-License-Identifier: Apache-2.0

//! The `pipeline_runs` table — one registry row per top-level operator
//! invocation.
//!
//! Every command that drives a pipeline (`dryrun`, `ingest`,
//! `distill_build`, `glean_review`, `papers_ingest`, …) opens one
//! `pipeline_runs` row at entry, copies the assigned `pipeline_run_id`
//! onto every audit row the run produces, and closes the row at exit
//! with a terminal `status`. Downstream rollups address one run by
//! `pipeline_run_id` and aggregate the audit rows tagged with it.
//!
//! The id is a short, human-readable composite — the command name, the
//! start instant as ISO-8601 UTC, and an 8-hex SHA-256 prefix over
//! `command || started_at || library_root` — so two same-second
//! invocations of the same command against different libraries do not
//! collide on the text primary key. The id-construction helper and the
//! open / close / compute APIs land in a later commit; this commit
//! ships only the table and its typed row pair so the schema position
//! is fixed before writers reach for it.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec};
use rusqlite::{OptionalExtension, Row, named_params};

use crate::{Catalog, Result};

/// The single source of truth for the `pipeline_runs` table's schema.
/// Its DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "pipeline_runs",
    comment: Some(
        "One registry row per top-level operator invocation; downstream audit rows carry its pipeline_run_id.",
    ),
    columns: &[
        ColumnSpec::text("pipeline_run_id")
            .primary_key()
            .comment("composite id: '<command>-<ISO8601 UTC>-<sha8>'"),
        ColumnSpec::text("command").not_null().comment(
            "top-level action: dryrun / ingest / distill_build / glean_review / papers_ingest",
        ),
        ColumnSpec::text("command_args").comment("JSON invocation snapshot; no stdout or stderr"),
        ColumnSpec::text("library_root").comment("short name when known; absolute path otherwise"),
        ColumnSpec::text("started_at")
            .not_null()
            .comment("ISO-8601 UTC"),
        ColumnSpec::text("finished_at").comment("ISO-8601 UTC; NULL while the run is in progress"),
        ColumnSpec::text("status").comment("running / ok / partial / error"),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[IndexSpec::on(
        "idx_pipeline_runs_cmd_ts",
        &["command", "started_at"],
    )],
};

/// Insert one `pipeline_runs` row.
const INSERT_SQL: &str = "INSERT INTO pipeline_runs \
     (pipeline_run_id, command, command_args, library_root, started_at, finished_at, status) \
     VALUES (:pipeline_run_id, :command, :command_args, :library_root, :started_at, :finished_at, :status)";

/// A `SELECT` of every column with `tail` appended; column list from [`SPEC`].
fn select_sql(tail: &str) -> String {
    format!("SELECT {} FROM pipeline_runs {tail}", SPEC.select_list())
}

/// One row about to be written to `pipeline_runs`.
#[derive(Debug, Clone)]
pub struct NewPipelineRun {
    /// Composite id: `<command>-<ISO8601 UTC>-<sha8>`.
    pub pipeline_run_id: String,
    /// Top-level action name, hyphen-cased.
    pub command: String,
    /// Optional JSON invocation snapshot.
    pub command_args: Option<String>,
    /// Library short name when known; absolute path otherwise.
    pub library_root: Option<String>,
    /// When the run started, ISO-8601 UTC.
    pub started_at: String,
    /// When the run returned, ISO-8601 UTC, or `None` while running.
    pub finished_at: Option<String>,
    /// `running` / `ok` / `partial` / `error`, or `None` if unset.
    pub status: Option<String>,
}

/// One `pipeline_runs` row, read back from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineRun {
    /// See [`NewPipelineRun::pipeline_run_id`].
    pub pipeline_run_id: String,
    /// See [`NewPipelineRun::command`].
    pub command: String,
    /// See [`NewPipelineRun::command_args`].
    pub command_args: Option<String>,
    /// See [`NewPipelineRun::library_root`].
    pub library_root: Option<String>,
    /// See [`NewPipelineRun::started_at`].
    pub started_at: String,
    /// See [`NewPipelineRun::finished_at`].
    pub finished_at: Option<String>,
    /// See [`NewPipelineRun::status`].
    pub status: Option<String>,
}

impl PipelineRun {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<PipelineRun> {
        Ok(PipelineRun {
            pipeline_run_id: row.get("pipeline_run_id")?,
            command: row.get("command")?,
            command_args: row.get("command_args")?,
            library_root: row.get("library_root")?,
            started_at: row.get("started_at")?,
            finished_at: row.get("finished_at")?,
            status: row.get("status")?,
        })
    }
}

impl Catalog {
    /// Insert one `pipeline_runs` row verbatim. The id-constructing
    /// `open_pipeline_run` wrapper builds on this primitive.
    pub fn insert_pipeline_run(&self, row: &NewPipelineRun) -> Result<()> {
        self.conn.execute(
            INSERT_SQL,
            named_params! {
                ":pipeline_run_id": row.pipeline_run_id,
                ":command": row.command,
                ":command_args": row.command_args,
                ":library_root": row.library_root,
                ":started_at": row.started_at,
                ":finished_at": row.finished_at,
                ":status": row.status,
            },
        )?;
        Ok(())
    }

    /// Fetch one `pipeline_runs` row by id, or `None` if the id is unknown.
    pub fn pipeline_run(&self, pipeline_run_id: &str) -> Result<Option<PipelineRun>> {
        let mut stmt = self
            .conn
            .prepare(&select_sql("WHERE pipeline_run_id = :pipeline_run_id"))?;
        let row = stmt
            .query_row(
                named_params! { ":pipeline_run_id": pipeline_run_id },
                PipelineRun::from_row,
            )
            .optional()?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(run_id: &str) -> NewPipelineRun {
        NewPipelineRun {
            pipeline_run_id: run_id.to_string(),
            command: "distill_build".to_string(),
            command_args: Some(r#"{"book":"tiny"}"#.to_string()),
            library_root: Some("lib-a".to_string()),
            started_at: "2026-06-28T10:00:00Z".to_string(),
            finished_at: Some("2026-06-28T10:00:05Z".to_string()),
            status: Some("ok".to_string()),
        }
    }

    #[test]
    fn pipeline_runs_insert_then_select_round_trip() {
        let catalog = Catalog::open_in_memory().expect("open");
        let row = fixture("distill_build-2026-06-28T10:00:00Z-deadbeef");
        catalog.insert_pipeline_run(&row).expect("insert");

        let read = catalog
            .pipeline_run(&row.pipeline_run_id)
            .expect("read")
            .expect("present");
        assert_eq!(read.pipeline_run_id, row.pipeline_run_id);
        assert_eq!(read.command, "distill_build");
        assert_eq!(read.command_args.as_deref(), Some(r#"{"book":"tiny"}"#));
        assert_eq!(read.library_root.as_deref(), Some("lib-a"));
        assert_eq!(read.started_at, "2026-06-28T10:00:00Z");
        assert_eq!(read.finished_at.as_deref(), Some("2026-06-28T10:00:05Z"));
        assert_eq!(read.status.as_deref(), Some("ok"));
    }

    #[test]
    fn pipeline_runs_with_nullable_columns_left_unset_persist_as_none() {
        let catalog = Catalog::open_in_memory().expect("open");
        let row = NewPipelineRun {
            pipeline_run_id: "dryrun-2026-06-28T10:00:00Z-cafef00d".to_string(),
            command: "dryrun".to_string(),
            command_args: None,
            library_root: None,
            started_at: "2026-06-28T10:00:00Z".to_string(),
            finished_at: None,
            status: Some("running".to_string()),
        };
        catalog.insert_pipeline_run(&row).expect("insert");

        let read = catalog
            .pipeline_run(&row.pipeline_run_id)
            .expect("read")
            .expect("present");
        assert_eq!(read.command_args, None);
        assert_eq!(read.library_root, None);
        assert_eq!(read.finished_at, None);
        assert_eq!(read.status.as_deref(), Some("running"));
    }
}
