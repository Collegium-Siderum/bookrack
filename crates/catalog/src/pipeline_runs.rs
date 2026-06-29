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
//! `command|started_at|library_root` — so two same-second invocations
//! of the same command against different libraries do not collide on
//! the text primary key. [`Catalog::open_pipeline_run`] constructs the
//! id and inserts the row with `status = 'running'`;
//! [`Catalog::close_pipeline_run`] stamps `finished_at` and the terminal
//! status. Both reach for SQLite's `strftime` for the timestamp so the
//! catalog crate never grows a wall-clock dependency.

use bookrack_dbkit::{ColumnSpec, IndexSpec, TableSpec};
use rusqlite::{OptionalExtension, Row, named_params};
use sha2::{Digest, Sha256};

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

    /// List `pipeline_runs` rows, newest first. When `command_filter`
    /// is set, only rows whose `command` matches are returned; when
    /// `last` is set, the result is capped at that many rows.
    pub fn list_pipeline_runs(
        &self,
        command_filter: Option<&str>,
        last: Option<usize>,
    ) -> Result<Vec<PipelineRun>> {
        let mut sql = format!("SELECT {} FROM pipeline_runs", SPEC.select_list());
        if command_filter.is_some() {
            sql.push_str(" WHERE command = :command");
        }
        sql.push_str(" ORDER BY started_at DESC, pipeline_run_id DESC");
        if let Some(limit) = last {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<PipelineRun> = if let Some(command) = command_filter {
            stmt.query_map(named_params! { ":command": command }, PipelineRun::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map([], PipelineRun::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
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

    /// Open a new pipeline run: construct the composite id, write a
    /// `status = 'running'` row, and return the assigned id. The
    /// `started_at` instant comes from SQLite's `strftime('%Y-%m-%dT%H:%M:%SZ', 'now')`
    /// so the catalog crate stays wall-clock-free.
    pub fn open_pipeline_run(
        &self,
        command: &str,
        args: Option<&str>,
        library_root: Option<&str>,
    ) -> Result<String> {
        let started_at: String =
            self.conn
                .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |row| {
                    row.get(0)
                })?;
        let mut hasher = Sha256::new();
        hasher.update(command.as_bytes());
        hasher.update(b"|");
        hasher.update(started_at.as_bytes());
        hasher.update(b"|");
        hasher.update(library_root.unwrap_or("").as_bytes());
        let digest = hasher.finalize();
        let sha8: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        let pipeline_run_id = format!("{command}-{started_at}-{sha8}");
        self.insert_pipeline_run(&NewPipelineRun {
            pipeline_run_id: pipeline_run_id.clone(),
            command: command.to_string(),
            command_args: args.map(str::to_string),
            library_root: library_root.map(str::to_string),
            started_at,
            finished_at: None,
            status: Some("running".to_string()),
        })?;
        Ok(pipeline_run_id)
    }

    /// Close a pipeline run: stamp `finished_at` from SQLite's clock and
    /// set the terminal `status`. A non-matching id is a no-op rather
    /// than an error, since close is best-effort during shutdown paths.
    pub fn close_pipeline_run(&self, pipeline_run_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pipeline_runs \
             SET finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), \
                 status = :status \
             WHERE pipeline_run_id = :pipeline_run_id",
            named_params! {
                ":pipeline_run_id": pipeline_run_id,
                ":status": status,
            },
        )?;
        Ok(())
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
    fn open_then_close_pipeline_run_round_trip() {
        let catalog = Catalog::open_in_memory().expect("open");
        let id = catalog
            .open_pipeline_run("distill_build", Some(r#"{"book":"tiny"}"#), Some("lib-a"))
            .expect("open");
        // The id ends with an 8-hex sha prefix; everything before the
        // last hyphen is `<command>-<started_at>` and starts with the
        // command name verbatim.
        assert!(id.starts_with("distill_build-"));
        let (head, sha8) = id.rsplit_once('-').expect("composite id");
        assert_eq!(sha8.len(), 8);
        assert!(sha8.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(head.starts_with("distill_build-"));

        let opened = catalog.pipeline_run(&id).expect("read").expect("present");
        assert_eq!(opened.command, "distill_build");
        assert_eq!(opened.command_args.as_deref(), Some(r#"{"book":"tiny"}"#));
        assert_eq!(opened.library_root.as_deref(), Some("lib-a"));
        assert_eq!(opened.status.as_deref(), Some("running"));
        assert_eq!(opened.finished_at, None);
        assert_eq!(opened.started_at, head["distill_build-".len()..]);

        catalog.close_pipeline_run(&id, "ok").expect("close");
        let closed = catalog.pipeline_run(&id).expect("read").expect("present");
        assert_eq!(closed.status.as_deref(), Some("ok"));
        assert!(closed.finished_at.is_some());
    }

    #[test]
    fn open_pipeline_run_differs_by_library_root() {
        let catalog = Catalog::open_in_memory().expect("open");
        let a = catalog
            .open_pipeline_run("distill_build", None, Some("lib-a"))
            .expect("open a");
        let b = catalog
            .open_pipeline_run("distill_build", None, Some("lib-b"))
            .expect("open b");
        // Even if SQLite reports the same second for both, the library
        // root's contribution to the sha prefix keeps the ids distinct.
        let sha_a = a.rsplit_once('-').expect("a sha").1;
        let sha_b = b.rsplit_once('-').expect("b sha").1;
        if a.trim_end_matches(sha_a) == b.trim_end_matches(sha_b) {
            assert_ne!(sha_a, sha_b);
        }
    }

    #[test]
    fn list_pipeline_runs_filters_by_command_and_caps_at_last() {
        let catalog = Catalog::open_in_memory().expect("open");
        let mut row_a = fixture("distill_build-2026-06-28T10:00:00Z-a");
        row_a.command = "distill_build".to_string();
        row_a.started_at = "2026-06-28T10:00:00Z".to_string();
        let mut row_b = fixture("distill_build-2026-06-28T11:00:00Z-b");
        row_b.command = "distill_build".to_string();
        row_b.started_at = "2026-06-28T11:00:00Z".to_string();
        let mut row_c = fixture("ingest-2026-06-28T12:00:00Z-c");
        row_c.command = "ingest".to_string();
        row_c.started_at = "2026-06-28T12:00:00Z".to_string();
        for row in [&row_a, &row_b, &row_c] {
            catalog.insert_pipeline_run(row).expect("insert");
        }

        // No filter, no cap: every row, newest first.
        let all = catalog.list_pipeline_runs(None, None).expect("list");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].pipeline_run_id, row_c.pipeline_run_id);
        assert_eq!(all[2].pipeline_run_id, row_a.pipeline_run_id);

        // Command filter narrows to distill_build, newest first.
        let distill = catalog
            .list_pipeline_runs(Some("distill_build"), None)
            .expect("list distill");
        assert_eq!(distill.len(), 2);
        assert_eq!(distill[0].pipeline_run_id, row_b.pipeline_run_id);

        // `last` caps after sorting.
        let one = catalog.list_pipeline_runs(None, Some(1)).expect("last 1");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].pipeline_run_id, row_c.pipeline_run_id);
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
