// SPDX-License-Identifier: Apache-2.0

//! Lifecycle helpers for the `pipeline_runs` registry, in the tier the
//! commands that keep no rollup use: register the run, hold its
//! liveness record, close it with a terminal status, compute no rollup.
//!
//! The registry covers commands that drive a pipeline over a set of
//! items — `ingest`, `dryrun`, `distill_build`, `glean`, and the
//! `reembed` / `reset` passes on either side. Per-item maintenance
//! verbs stay out of it: `metadata reaudit` recomputes one rollup for
//! one intake and would turn `bookrack runs list` into a log of
//! single-row edits. Their audit trail is the `<op>-<nanos>` run id on
//! `item_pipeline_audit`, which is a separate namespace from this
//! registry and is not affected by either choice.
//!
//! No rollup is computed here. `pipeline_run_summary` aggregates
//! `book_distill_audit` / `node_paper_audit` rows tagged with the
//! registry's `pipeline_run_id`, and these commands write neither; a
//! rollup would be empty. `bookrack runs show` renders such a run as
//! header-only.
//!
//! Both legs are best-effort by design: audit bookkeeping must not fail
//! the work it records. A failure logs and the work proceeds with no
//! run id. What the liveness record adds is that an unclosed row can
//! afterwards be told apart from a live one — see
//! [`bookrack_catalog::run_liveness`].

use std::path::Path;

pub use bookrack_catalog::RunHandle;
use bookrack_catalog::{Catalog, RunLock};

/// Open a `pipeline_runs` row on an already-open catalog and take the
/// run's liveness lock beside `catalog_db`. `command` is the registry's
/// name for the work; `library_root` is a short name when known and an
/// absolute path otherwise.
///
/// A row that opens but cannot take a lock is returned all the same:
/// the run is registered, it simply keeps no liveness record, and a
/// later probe reports [`bookrack_catalog::RunLiveness::NoRecord`]
/// rather than claiming it died.
pub fn open_run(
    catalog: &Catalog,
    catalog_db: &Path,
    command: &str,
    library_root: Option<&str>,
) -> Option<RunHandle> {
    let pipeline_run_id = match catalog.open_pipeline_run(command, None, library_root) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(error = %err, command, "open_pipeline_run failed");
            return None;
        }
    };
    let lock = acquire_run_lock(catalog_db, &pipeline_run_id, command);
    Some(RunHandle::new(pipeline_run_id, lock))
}

/// Take one run's liveness lock, demoting every failure to a warning:
/// the record is evidence for a later repair, never a precondition for
/// doing the work.
pub fn acquire_run_lock(
    catalog_db: &Path,
    pipeline_run_id: &str,
    command: &str,
) -> Option<RunLock> {
    let dir = bookrack_catalog::run_locks_dir(catalog_db)?;
    match RunLock::acquire(&dir, pipeline_run_id) {
        Ok(lock) => Some(lock),
        Err(err) => {
            tracing::warn!(
                error = %err,
                command,
                pipeline_run_id,
                "run liveness record could not be taken",
            );
            None
        }
    }
}

/// Close a run's row with `ok` / `error`. A `None` handle means the
/// open leg did not get one; there is nothing to close. The handle is
/// consumed here rather than earlier so the liveness record outlives
/// the terminal status it vouches for.
pub fn close_run(catalog: &Catalog, handle: Option<RunHandle>, ok: bool) {
    let Some(handle) = handle else {
        return;
    };
    let status = if ok { "ok" } else { "error" };
    if let Err(err) = catalog.close_pipeline_run(handle.id(), status) {
        tracing::warn!(
            error = %err,
            pipeline_run_id = handle.id(),
            "close_pipeline_run failed",
        );
        handle.abandon();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::{RunLiveness, run_liveness, run_locks_dir};

    /// The open leg writes a row the close leg can then stamp; the
    /// pair round-trips through the real registry rather than through
    /// a hand-built row.
    #[test]
    fn open_then_close_stamps_a_terminal_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog_db = dir.path().join("catalog.db");
        let catalog = Catalog::open(&catalog_db).expect("open catalog");
        let handle = open_run(&catalog, &catalog_db, "reembed", Some("lib-a")).expect("run id");
        let id = handle.id().to_string();

        let opened = catalog
            .pipeline_run(&id)
            .expect("read")
            .expect("row present");
        assert_eq!(opened.command, "reembed");
        assert_eq!(opened.status.as_deref(), Some("running"));
        assert!(opened.finished_at.is_none());

        close_run(&catalog, Some(handle), false);
        let closed = catalog
            .pipeline_run(&id)
            .expect("read")
            .expect("row present");
        assert_eq!(closed.status.as_deref(), Some("error"));
        assert!(closed.finished_at.is_some());
    }

    /// A pass whose open leg failed carries `None`, and closing it is
    /// a no-op rather than a panic or a stray row.
    #[test]
    fn closing_without_a_run_id_writes_nothing() {
        let catalog = Catalog::open_in_memory().expect("in-memory catalog");
        close_run(&catalog, None, true);
        assert!(
            catalog
                .list_pipeline_runs(None, None)
                .expect("list")
                .is_empty(),
        );
    }

    /// While the run is open its record reads as held; closing it
    /// through the helper releases the record along with the handle,
    /// so a finished run leaves nothing behind to be reaped.
    #[test]
    fn the_liveness_record_lives_exactly_as_long_as_the_open_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog_db = dir.path().join("catalog.db");
        let catalog = Catalog::open(&catalog_db).expect("open catalog");
        let locks = run_locks_dir(&catalog_db).expect("lock dir");

        let handle = open_run(&catalog, &catalog_db, "ingest", None).expect("run id");
        let id = handle.id().to_string();
        assert_eq!(run_liveness(&locks, &id), RunLiveness::Held);

        close_run(&catalog, Some(handle), true);
        assert_eq!(run_liveness(&locks, &id), RunLiveness::NoRecord);
    }

    /// Dropping the handle without closing — what unwinding out of a
    /// command or cancelling its future does — leaves the row open.
    /// The repair, not the drop, is what resolves it.
    #[test]
    fn a_run_dropped_without_closing_leaves_a_running_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog_db = dir.path().join("catalog.db");
        let catalog = Catalog::open(&catalog_db).expect("open catalog");

        let handle = open_run(&catalog, &catalog_db, "dryrun", None).expect("run id");
        let id = handle.id().to_string();
        drop(handle);

        let row = catalog.pipeline_run(&id).expect("read").expect("present");
        assert_eq!(row.status.as_deref(), Some("running"));
        assert!(row.finished_at.is_none());
    }
}
