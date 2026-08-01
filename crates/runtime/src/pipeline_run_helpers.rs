// SPDX-License-Identifier: Apache-2.0

//! Lifecycle helpers for the `pipeline_runs` registry, in the tier the
//! whole-library maintenance passes use: register the run, close it
//! with a terminal status, compute no rollup.
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
//! registry's `pipeline_run_id`, and the passes write neither; a
//! rollup would be empty. `bookrack runs show` renders such a run as
//! header-only, the same way it does for `ingest` and `dryrun`.
//!
//! Both helpers are best-effort by design: audit bookkeeping must not
//! fail a pass that is otherwise doing its work. A failure logs and
//! the pass proceeds with no run id.

use bookrack_catalog::Catalog;

/// Open a `pipeline_runs` row for a maintenance pass on an
/// already-open catalog. `command` is the registry's name for the
/// pass; `library_root` is a short name when known and an absolute
/// path otherwise.
pub fn open_pass_run(
    catalog: &Catalog,
    command: &str,
    library_root: Option<&str>,
) -> Option<String> {
    match catalog.open_pipeline_run(command, None, library_root) {
        Ok(id) => Some(id),
        Err(err) => {
            tracing::warn!(error = %err, command, "open_pipeline_run failed");
            None
        }
    }
}

/// Close a pass's `pipeline_runs` row with `ok` / `error`. A `None`
/// run id means the open leg did not get one; there is nothing to
/// close.
pub fn close_pass_run(catalog: &Catalog, pipeline_run_id: Option<&str>, ok: bool) {
    let Some(id) = pipeline_run_id else {
        return;
    };
    let status = if ok { "ok" } else { "error" };
    if let Err(err) = catalog.close_pipeline_run(id, status) {
        tracing::warn!(error = %err, pipeline_run_id = id, "close_pipeline_run failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The open leg writes a row the close leg can then stamp; the
    /// pair round-trips through the real registry rather than through
    /// a hand-built row.
    #[test]
    fn open_then_close_stamps_a_terminal_status() {
        let catalog = Catalog::open_in_memory().expect("in-memory catalog");
        let id = open_pass_run(&catalog, "reembed", Some("lib-a")).expect("run id");

        let opened = catalog
            .pipeline_run(&id)
            .expect("read")
            .expect("row present");
        assert_eq!(opened.command, "reembed");
        assert_eq!(opened.status.as_deref(), Some("running"));
        assert!(opened.finished_at.is_none());

        close_pass_run(&catalog, Some(&id), false);
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
        close_pass_run(&catalog, None, true);
        assert!(
            catalog
                .list_pipeline_runs(None, None)
                .expect("list")
                .is_empty(),
        );
    }
}
