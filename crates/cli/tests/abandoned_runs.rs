// SPDX-License-Identifier: Apache-2.0

//! `bookrack doctor --close-abandoned-runs` against a real data root.
//!
//! A `pipeline_runs` row is opened `running` and closed best-effort, so
//! a command whose process dies leaves a row that reads exactly like a
//! run in flight. The repair resolves those rows, and the whole of its
//! correctness is which rows it declines to touch: closing a run that
//! is still working would report a live command as stopped.
//!
//! The liveness question is a cross-process one, and that is how it is
//! asked here. The test process holds a real lock on one run's record
//! while the `bookrack` binary runs the repair in a child process, so
//! the "still owned" verdict is produced by two processes disagreeing
//! about a file, not by a value constructed in the assertion.

#![cfg(unix)]

use std::path::Path;

use bookrack_catalog::{Catalog, RunLock, run_lock_path, run_locks_dir};
use bookrack_test_support::{Sandbox, bookrack_cmd};
use tokio::process::Command;

/// Open a `running` row on a real catalog under `root` and return its id.
fn open_run(root: &Path, command: &str) -> String {
    let catalog = Catalog::open(&root.join("catalog.db")).expect("open catalog");
    catalog
        .open_pipeline_run(command, None, None)
        .expect("open pipeline run")
}

/// Leave behind what a killed process leaves: the record file with no
/// holder. The run's owner would have removed it on an orderly exit.
fn leave_ownerless_record(root: &Path, pipeline_run_id: &str) {
    let locks = run_locks_dir(&root.join("catalog.db")).expect("lock dir");
    std::fs::create_dir_all(&locks).expect("create lock dir");
    std::fs::write(run_lock_path(&locks, pipeline_run_id), b"").expect("write record");
}

fn status_of(root: &Path, pipeline_run_id: &str) -> String {
    Catalog::open_read_only(&root.join("catalog.db"))
        .expect("open catalog")
        .pipeline_run(pipeline_run_id)
        .expect("read run")
        .expect("row present")
        .status
        .expect("status")
}

/// Run the repair through the real binary against `root`.
async fn close_abandoned_runs(sandbox: &Sandbox, root: &Path, extra: &[&str]) -> (i32, String) {
    let mut args = vec!["doctor", "--close-abandoned-runs"];
    args.extend_from_slice(extra);
    let output = Command::from(bookrack_cmd!(sandbox).data_dir(root).build())
        .args(&args)
        .output()
        .await
        .expect("run bookrack doctor");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (output.status.code().unwrap_or(-1), text)
}

/// The row whose owner is gone is closed; the row this test process
/// still owns is left running. The second half is the one that matters:
/// the child sees the lock a different process holds.
#[tokio::test]
async fn the_repair_closes_the_ownerless_run_and_leaves_the_owned_one() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("lib");
    std::fs::create_dir_all(&root).expect("create data root");

    let live = open_run(&root, "ingest");
    let dead = open_run(&root, "distill_build");
    let locks = run_locks_dir(&root.join("catalog.db")).expect("lock dir");
    let _held = RunLock::acquire(&locks, &live).expect("this process owns the live run");
    leave_ownerless_record(&root, &dead);

    let (code, text) = close_abandoned_runs(&sandbox, &root, &[]).await;
    assert_eq!(code, 0, "the repair reports success: {text}");
    assert!(text.contains(&dead), "the closed run is named: {text}");
    assert!(
        !text.contains(&live),
        "a run this process still owns must not be closed: {text}"
    );

    assert_eq!(status_of(&root, &dead), "abandoned");
    assert_eq!(status_of(&root, &live), "running");
    // The record that licensed closing the row is gone with it, so a
    // second pass finds nothing to do.
    assert!(!run_lock_path(&locks, &dead).exists());
    assert!(run_lock_path(&locks, &live).exists());
}

/// A run that kept no record at all is not evidence of anything, and
/// the repair says so instead of closing it.
#[tokio::test]
async fn a_run_without_a_record_is_left_alone() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("lib");
    std::fs::create_dir_all(&root).expect("create data root");
    let unjudged = open_run(&root, "dryrun");

    let (code, text) = close_abandoned_runs(&sandbox, &root, &[]).await;
    assert_eq!(code, 0, "{text}");
    assert_eq!(status_of(&root, &unjudged), "running");
    assert!(
        text.contains("1 without a liveness record"),
        "the report accounts for what it declined to judge: {text}"
    );
}

/// The dry run prints the same plan and writes none of it.
#[tokio::test]
async fn the_dry_run_writes_nothing() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("lib");
    std::fs::create_dir_all(&root).expect("create data root");
    let dead = open_run(&root, "glean");
    leave_ownerless_record(&root, &dead);

    let (code, text) = close_abandoned_runs(&sandbox, &root, &["--dry-run"]).await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("would close"), "{text}");
    assert!(text.contains(&dead), "{text}");
    assert_eq!(status_of(&root, &dead), "running");
}

/// `runs list` marks the same row the repair would close, so the two
/// surfaces cannot disagree about which runs are still alive.
#[tokio::test]
async fn runs_list_marks_what_the_repair_would_close() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("lib");
    std::fs::create_dir_all(&root).expect("create data root");
    let live = open_run(&root, "ingest");
    let dead = open_run(&root, "distill_build");
    let locks = run_locks_dir(&root.join("catalog.db")).expect("lock dir");
    let _held = RunLock::acquire(&locks, &live).expect("this process owns the live run");
    leave_ownerless_record(&root, &dead);

    let output = Command::from(bookrack_cmd!(&sandbox).data_dir(&root).build())
        .args(["runs", "list"])
        .output()
        .await
        .expect("run bookrack runs list");
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    let dead_line = text
        .lines()
        .find(|line| line.contains(&dead))
        .unwrap_or_else(|| panic!("the ownerless run is listed: {text}"));
    assert!(dead_line.contains("abandoned?"), "{dead_line}");
    let live_line = text
        .lines()
        .find(|line| line.contains(&live))
        .unwrap_or_else(|| panic!("the owned run is listed: {text}"));
    assert!(
        live_line.contains("running") && !live_line.contains("abandoned"),
        "{live_line}"
    );
    assert!(
        text.contains("bookrack doctor --close-abandoned-runs"),
        "the table names the repair: {text}"
    );
}
