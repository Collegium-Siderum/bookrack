// SPDX-License-Identifier: Apache-2.0

//! The registry's open rows, read together with the liveness records
//! that say whether anyone still owns them.
//!
//! A `pipeline_runs` row reading `running` means one of two things: a
//! command is working, or a command died before it could stamp the row.
//! The table cannot tell them apart — both look identical in `bookrack
//! runs list` — and nothing else in the workspace ever revisits such a
//! row. This module supplies the second half of the answer by probing
//! each open run's record (see [`bookrack_catalog::run_liveness`]), so
//! `bookrack doctor` can report the difference and the operator can act
//! on it.
//!
//! Both catalogs under a data root carry a registry — book-side
//! commands register in `catalog.db`, the paper pipeline in
//! `papers_catalog.db` — and both are surveyed. Each opens through its
//! read-only door, so a survey is safe beside a running daemon and
//! never materialises a database that is not already there.

use std::path::{Path, PathBuf};

use bookrack_catalog::{Catalog, RunLiveness, run_liveness, run_locks_dir};
use bookrack_config::Config;

/// One `pipeline_runs` row still reading `running`, with the verdict of
/// its liveness record.
#[derive(Debug, Clone)]
pub struct OpenRun {
    /// The row's id.
    pub pipeline_run_id: String,
    /// The command that opened it.
    pub command: String,
    /// When it opened, ISO-8601 UTC.
    pub started_at: String,
    /// What its liveness record says about its owner.
    pub liveness: RunLiveness,
    /// The catalog the row lives in, which is also what locates its
    /// record and what a repair would write to.
    pub catalog_db: PathBuf,
}

impl OpenRun {
    /// `true` when the record proves nobody owns this run any more.
    pub fn is_abandoned(&self) -> bool {
        self.liveness == RunLiveness::Abandoned
    }
}

/// Every open run under one data root, plus the catalogs that could not
/// be read.
#[derive(Debug, Clone, Default)]
pub struct OpenRunSurvey {
    /// Open rows across both catalogs, newest first within each.
    pub runs: Vec<OpenRun>,
    /// Catalogs present on disk that would not open, with the reason.
    /// Their rows are missing from `runs`, so a caller that counts must
    /// say the count is partial.
    pub unreadable: Vec<(PathBuf, String)>,
}

impl OpenRunSurvey {
    /// Runs a repair may close: their owner is provably gone.
    pub fn abandoned(&self) -> impl Iterator<Item = &OpenRun> {
        self.runs.iter().filter(|run| run.is_abandoned())
    }

    /// Runs still owned by a live process.
    pub fn held(&self) -> usize {
        self.count(RunLiveness::Held)
    }

    /// Runs with no liveness record at all — opened before records
    /// existed, or by an owner that could not write one. Nothing can be
    /// concluded about them, and no repair touches them.
    pub fn unjudged(&self) -> usize {
        self.count(RunLiveness::NoRecord)
    }

    fn count(&self, liveness: RunLiveness) -> usize {
        self.runs
            .iter()
            .filter(|run| run.liveness == liveness)
            .count()
    }
}

/// Survey both catalogs under the data root.
pub fn survey(cfg: &Config) -> OpenRunSurvey {
    let mut survey = OpenRunSurvey::default();
    for path in [cfg.catalog_db(), cfg.papers_catalog_db()] {
        if !path.exists() {
            continue;
        }
        survey_catalog(&path, &mut survey);
    }
    survey
}

/// Read one catalog's open rows into `survey`, recording the catalog as
/// unreadable when it will not open or its registry will not list.
fn survey_catalog(catalog_db: &Path, survey: &mut OpenRunSurvey) {
    let catalog = match Catalog::open_read_only(catalog_db) {
        Ok(catalog) => catalog,
        Err(err) => {
            survey
                .unreadable
                .push((catalog_db.to_path_buf(), bookrack_core::error_chain(&err)));
            return;
        }
    };
    let rows = match catalog.list_open_pipeline_runs() {
        Ok(rows) => rows,
        Err(err) => {
            survey
                .unreadable
                .push((catalog_db.to_path_buf(), bookrack_core::error_chain(&err)));
            return;
        }
    };
    let locks = run_locks_dir(catalog_db);
    for row in rows {
        let liveness = match &locks {
            Some(dir) => run_liveness(dir, &row.pipeline_run_id),
            // Without a directory to hold records there is nothing to
            // read, which is the same information as no record.
            None => RunLiveness::NoRecord,
        };
        survey.runs.push(OpenRun {
            pipeline_run_id: row.pipeline_run_id,
            command: row.command,
            started_at: row.started_at,
            liveness,
            catalog_db: catalog_db.to_path_buf(),
        });
    }
}

/// One run the repair closed, or would close on a real pass.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClosedRun {
    /// The row's id.
    pub pipeline_run_id: String,
    /// The command that opened it.
    pub command: String,
    /// When it opened, ISO-8601 UTC.
    pub started_at: String,
}

/// One run the repair could not close, with the reason.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CloseFailure {
    /// The row's id.
    pub pipeline_run_id: String,
    /// Flattened cause chain of the failure.
    pub reason: String,
}

/// Outcome of one `--close-abandoned-runs` pass, rendered verbatim so
/// an operator can audit what moved.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CloseAbandonedReport {
    /// `true` when nothing was written and `closed` carries the plan.
    pub dry_run: bool,
    /// Rows stamped `abandoned`, in survey order.
    pub closed: Vec<ClosedRun>,
    /// Rows that could not be stamped. One failure does not stop the
    /// rest of the pass.
    pub failures: Vec<CloseFailure>,
    /// Open runs left alone because a live process still owns them.
    pub left_running: usize,
    /// Open runs left alone because they carry no liveness record, so
    /// nothing proves their owner is gone.
    pub left_unjudged: usize,
}

impl CloseAbandonedReport {
    /// `true` iff any row failed to close.
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// Stamp every provably abandoned run with
/// [`bookrack_catalog::RUN_STATUS_ABANDONED`] and drop its liveness
/// record.
///
/// Only rows whose record exists and is unheld are touched: a run still
/// owned by a live process, and one that kept no record at all, are
/// both left exactly as they are. `abandoned` records that the run
/// stopped, never that its work succeeded or failed — that outcome died
/// with the process and is not recoverable from the row.
///
/// This writes the catalog, so callers run it with no daemon serving
/// the library.
pub fn close_abandoned_runs(cfg: &Config, dry_run: bool) -> CloseAbandonedReport {
    let survey = survey(cfg);
    let mut report = CloseAbandonedReport {
        dry_run,
        left_running: survey.held(),
        left_unjudged: survey.unjudged(),
        ..CloseAbandonedReport::default()
    };
    for run in survey.abandoned() {
        let closed = ClosedRun {
            pipeline_run_id: run.pipeline_run_id.clone(),
            command: run.command.clone(),
            started_at: run.started_at.clone(),
        };
        if dry_run {
            report.closed.push(closed);
            continue;
        }
        match close_one(run) {
            Ok(()) => report.closed.push(closed),
            Err(reason) => report.failures.push(CloseFailure {
                pipeline_run_id: run.pipeline_run_id.clone(),
                reason,
            }),
        }
    }
    report
}

/// Render one pass for an operator. The JSON view emits the report
/// verbatim; the text view names every row it moved, and says what it
/// left alone and why, so a pass that closes nothing is still readable.
pub fn render_close_report(report: &CloseAbandonedReport, json: bool) {
    if json {
        let v = serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string());
        println!("{v}");
        return;
    }
    let mode = if report.dry_run { " (plan)" } else { "" };
    println!(
        "Abandoned pipeline runs{mode}: {} closed, {} still running, \
         {} without a liveness record",
        report.closed.len(),
        report.left_running,
        report.left_unjudged,
    );
    for run in &report.closed {
        let verb = if report.dry_run {
            "would close"
        } else {
            "closed"
        };
        println!(
            "  {verb} {} ({}, opened {})",
            run.pipeline_run_id, run.command, run.started_at,
        );
    }
    for failure in &report.failures {
        println!("  FAILED {} ({})", failure.pipeline_run_id, failure.reason);
    }
}

/// Stamp one row and discard the record that licensed stamping it. The
/// record goes second: a failure between the two leaves the row closed
/// and a stray file, which the next survey reads as nothing at all,
/// where the reverse order would leave a `running` row no longer
/// provably abandoned.
fn close_one(run: &OpenRun) -> Result<(), String> {
    let catalog = Catalog::open(&run.catalog_db).map_err(|err| bookrack_core::error_chain(&err))?;
    catalog
        .close_pipeline_run(&run.pipeline_run_id, bookrack_catalog::RUN_STATUS_ABANDONED)
        .map_err(|err| bookrack_core::error_chain(&err))?;
    if let Some(dir) = run_locks_dir(&run.catalog_db) {
        let _ = bookrack_catalog::discard_run_lock(dir.as_path(), &run.pipeline_run_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::{RunLock, run_lock_path};

    fn cfg_at(root: &Path) -> Config {
        Config::new(root.to_path_buf(), "http://127.0.0.1:11434".to_string())
    }

    /// Leave behind the record a killed process leaves: the file
    /// without its holder.
    fn seed_abandoned_record(locks: &Path, pipeline_run_id: &str) {
        std::fs::create_dir_all(locks).expect("lock dir");
        std::fs::write(run_lock_path(locks, pipeline_run_id), b"")
            .expect("record without a holder");
    }

    /// Seed one `running` row and hand back its id.
    fn open_row(catalog_db: &Path, command: &str) -> String {
        let catalog = Catalog::open(catalog_db).expect("open catalog");
        catalog
            .open_pipeline_run(command, None, None)
            .expect("open run")
    }

    /// A data root with nothing in it surveys clean rather than erroring
    /// or materialising a catalog.
    #[test]
    fn an_empty_data_root_has_no_open_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let survey = survey(&cfg_at(dir.path()));
        assert!(survey.runs.is_empty());
        assert!(survey.unreadable.is_empty());
        assert!(!dir.path().join("catalog.db").exists());
    }

    /// The three verdicts are told apart on real rows: one run still
    /// held, one whose record was left behind by a vanished owner, and
    /// one that never had a record.
    #[test]
    fn open_runs_are_split_by_what_their_records_say() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog_db = dir.path().join("catalog.db");
        let locks = run_locks_dir(&catalog_db).expect("lock dir");

        let live = open_row(&catalog_db, "ingest");
        let _held = RunLock::acquire(&locks, &live).expect("hold the live run's record");
        let dead = open_row(&catalog_db, "distill_build");
        seed_abandoned_record(&locks, &dead);
        let unjudged = open_row(&catalog_db, "dryrun");

        let survey = survey(&cfg_at(dir.path()));
        assert_eq!(survey.runs.len(), 3);
        assert_eq!(survey.held(), 1);
        assert_eq!(survey.unjudged(), 1);
        let abandoned: Vec<&str> = survey
            .abandoned()
            .map(|run| run.pipeline_run_id.as_str())
            .collect();
        assert_eq!(abandoned, vec![dead.as_str()]);
        assert!(!abandoned.contains(&live.as_str()));
        assert!(!abandoned.contains(&unjudged.as_str()));
    }

    /// Closed rows are not open runs, whatever terminal status they
    /// carry.
    #[test]
    fn closed_runs_are_not_surveyed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog_db = dir.path().join("catalog.db");
        let id = open_row(&catalog_db, "ingest");
        Catalog::open(&catalog_db)
            .expect("reopen")
            .close_pipeline_run(&id, "error")
            .expect("close");

        assert!(survey(&cfg_at(dir.path())).runs.is_empty());
    }

    /// The repair closes exactly the abandoned row: the live run and
    /// the unjudged one keep saying `running`, which is the difference
    /// between a repair and a blanket sweep.
    #[test]
    fn only_the_abandoned_row_is_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog_db = dir.path().join("catalog.db");
        let locks = run_locks_dir(&catalog_db).expect("lock dir");

        let live = open_row(&catalog_db, "ingest");
        let _held = RunLock::acquire(&locks, &live).expect("hold the live run's record");
        let dead = open_row(&catalog_db, "distill_build");
        seed_abandoned_record(&locks, &dead);
        let unjudged = open_row(&catalog_db, "dryrun");

        let report = close_abandoned_runs(&cfg_at(dir.path()), false);
        assert!(!report.has_failures(), "failures: {:?}", report.failures);
        assert_eq!(report.closed.len(), 1);
        assert_eq!(report.closed[0].pipeline_run_id, dead);
        assert_eq!(report.closed[0].command, "distill_build");
        assert_eq!(report.left_running, 1);
        assert_eq!(report.left_unjudged, 1);

        let catalog = Catalog::open_read_only(&catalog_db).expect("reopen");
        let status = |id: &str| {
            catalog
                .pipeline_run(id)
                .expect("read")
                .expect("present")
                .status
                .expect("status")
        };
        assert_eq!(status(&dead), "abandoned");
        assert_eq!(status(&live), "running");
        assert_eq!(status(&unjudged), "running");
        // The closed row is stamped, and its record is gone with it.
        assert!(
            catalog
                .pipeline_run(&dead)
                .expect("read")
                .expect("present")
                .finished_at
                .is_some()
        );
        assert_eq!(run_liveness(&locks, &dead), RunLiveness::NoRecord);
    }

    /// A second pass has nothing left to do: the first one removed both
    /// the `running` status and the record that licensed touching it.
    #[test]
    fn closing_twice_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog_db = dir.path().join("catalog.db");
        let locks = run_locks_dir(&catalog_db).expect("lock dir");
        let dead = open_row(&catalog_db, "glean");
        seed_abandoned_record(&locks, &dead);

        assert_eq!(
            close_abandoned_runs(&cfg_at(dir.path()), false)
                .closed
                .len(),
            1
        );
        let second = close_abandoned_runs(&cfg_at(dir.path()), false);
        assert!(second.closed.is_empty());
        assert!(!second.has_failures());
    }

    /// The dry run reports the same plan and writes none of it.
    #[test]
    fn a_dry_run_plans_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog_db = dir.path().join("catalog.db");
        let locks = run_locks_dir(&catalog_db).expect("lock dir");
        let dead = open_row(&catalog_db, "papers_dryrun");
        seed_abandoned_record(&locks, &dead);

        let report = close_abandoned_runs(&cfg_at(dir.path()), true);
        assert!(report.dry_run);
        assert_eq!(report.closed.len(), 1);
        assert_eq!(report.closed[0].pipeline_run_id, dead);

        let catalog = Catalog::open_read_only(&catalog_db).expect("reopen");
        let row = catalog.pipeline_run(&dead).expect("read").expect("present");
        assert_eq!(row.status.as_deref(), Some("running"));
        assert!(row.finished_at.is_none());
        assert_eq!(run_liveness(&locks, &dead), RunLiveness::Abandoned);
    }

    /// Both catalogs under one root are surveyed, and each run points at
    /// the catalog a repair would have to write.
    #[test]
    fn both_catalogs_are_surveyed_and_each_run_names_its_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let books = dir.path().join("catalog.db");
        let papers = dir.path().join("papers_catalog.db");
        open_row(&books, "ingest");
        open_row(&papers, "glean");

        let survey = survey(&cfg_at(dir.path()));
        assert_eq!(survey.runs.len(), 2);
        let book_run = survey
            .runs
            .iter()
            .find(|run| run.command == "ingest")
            .expect("book-side run");
        assert_eq!(book_run.catalog_db, books);
        let paper_run = survey
            .runs
            .iter()
            .find(|run| run.command == "glean")
            .expect("paper-side run");
        assert_eq!(paper_run.catalog_db, papers);
    }
}
