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

use std::io::Write;
use std::time::Duration;

use bookrack_catalog::{Catalog, RunLock, run_lock_path, run_locks_dir};
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, bookrack_cmd, process_env};
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

/// A data root holding one ownerless run, so an unrefused dry run has a
/// plan to print.
fn root_with_a_dead_run(sandbox: &Sandbox) -> std::path::PathBuf {
    let root = sandbox.data_root("lib");
    std::fs::create_dir_all(&root).expect("create data root");
    let dead = open_run(&root, "glean");
    leave_ownerless_record(&root, &dead);
    root
}

/// Write session-lock content the way a daemon does. The returned file
/// holds the `flock` for as long as the caller keeps it, so dropping it
/// is what a daemon exiting looks like to the kernel.
fn write_lock(sandbox: &Sandbox, hold: bool, control_sock: Option<&str>) -> std::fs::File {
    let lock_path = sandbox.tty_lock_path();
    std::fs::create_dir_all(lock_path.parent().expect("runtime dir")).expect("runtime dir");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .expect("open lock");
    if hold {
        file.try_lock().expect("hold the lock");
    }
    let mut writer = &file;
    writeln!(writer, "pid=999999").expect("write");
    writeln!(writer, "mcp=disabled").expect("write");
    if let Some(sock) = control_sock {
        writeln!(writer, "control_sock={sock}").expect("write");
    }
    writer.flush().expect("flush");
    file
}

/// The offline repair's guard reads the session lock to decide whether
/// a daemon may be serving the library. A lock it cannot read is not
/// evidence of an absent daemon, so the repair is refused
/// (`SessionLockUnreadable`, exit 1) instead of opening the catalog.
///
/// The lock path is made a directory rather than unreadable by mode:
/// a `chmod` is inert for a root user, which is what CI runs as.
#[tokio::test]
async fn an_unreadable_session_lock_refuses_the_offline_repair() {
    let sandbox = Sandbox::new();
    let root = root_with_a_dead_run(&sandbox);
    std::fs::create_dir_all(sandbox.tty_lock_path()).expect("lock path as a directory");

    let (code, text) = close_abandoned_runs(&sandbox, &root, &["--dry-run"]).await;
    assert_eq!(code, 1, "an unreadable lock refuses the repair: {text}");
    assert!(
        text.contains("session lock"),
        "the refusal names the lock it could not read: {text}"
    );
    assert!(!text.contains("would close"), "no plan is computed: {text}");
}

/// A held lock whose control socket does not answer is the stale-lock
/// state the exit-code table assigns `3`: the operator finds the holder
/// first, and the repair does not run beside a daemon that may still be
/// holding the write handle.
#[tokio::test]
async fn a_stale_lock_refuses_the_offline_repair() {
    let sandbox = Sandbox::new();
    let root = root_with_a_dead_run(&sandbox);
    let missing = sandbox.runtime_dir().join("no-such-control.sock");
    let _held = write_lock(&sandbox, true, Some(&missing.display().to_string()));

    let (code, text) = close_abandoned_runs(&sandbox, &root, &["--dry-run"]).await;
    assert_eq!(code, 3, "a stale lock is exit 3: {text}");
    assert!(
        text.contains("is held, and the daemon it records does not answer"),
        "the summary states what the probe established: {text}"
    );
    let hint = text
        .lines()
        .find(|line| line.trim_start().starts_with("hint:"))
        .unwrap_or_else(|| panic!("no hint line in: {text}"));
    assert!(hint.contains("lsof"), "{hint}");
    assert!(!text.contains("rm "), "no removal step: {text}");
    assert!(!text.contains("would close"), "no plan is computed: {text}");
}

/// A held lock that names no control socket cannot be probed. The
/// status card treats that as no verdict and renders anyway; a write
/// guard cannot afford the same latitude and refuses.
#[tokio::test]
async fn an_unprobeable_held_lock_refuses_the_offline_repair() {
    let sandbox = Sandbox::new();
    let root = root_with_a_dead_run(&sandbox);
    let _held = write_lock(&sandbox, true, None);

    let (code, text) = close_abandoned_runs(&sandbox, &root, &["--dry-run"]).await;
    assert_eq!(
        code, 1,
        "an unprobeable held lock refuses the repair: {text}"
    );
    assert!(
        text.contains("control socket"),
        "the refusal says what the lock is missing: {text}"
    );
    assert!(!text.contains("would close"), "no plan is computed: {text}");
}

/// Lock content nobody holds is what a crashed daemon leaves behind.
/// The kernel released the `flock`, the next `bookrack run` takes over
/// without cleanup, and the offline repair proceeds the same way.
#[tokio::test]
async fn a_leftover_lock_nobody_holds_lets_the_repair_proceed() {
    let sandbox = Sandbox::new();
    let root = root_with_a_dead_run(&sandbox);
    let unheld = write_lock(&sandbox, false, Some("/nonexistent/control.sock"));
    drop(unheld);

    let (code, text) = close_abandoned_runs(&sandbox, &root, &["--dry-run"]).await;
    assert_eq!(code, 0, "a lock nobody holds is not a daemon: {text}");
    assert!(text.contains("would close"), "the plan is printed: {text}");
}

/// A daemon that answers on its control socket is serving the library,
/// and the refusal is operator input (exit 2): the repair was asked for
/// at the wrong time, not broken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_daemon_refuses_the_offline_repair_as_operator_input() {
    let sandbox = process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir().expect("data root");
    let runtime_root = tempfile::tempdir().expect("runtime root");

    let mut opts = RuntimeOpts::headless(Some(data_root.path().into()), None);
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().into());
    let runtime = DaemonRuntime::start(opts).await.expect("daemon starts");
    let shutdown_tx = runtime.shutdown_tx.clone();

    let runtime_dir_for_child = runtime_root.path().to_path_buf();
    let data_dir_for_child = data_root.path().to_path_buf();
    let child = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Command::from(
            bookrack_cmd!(sandbox)
                .runtime_dir(runtime_dir_for_child)
                .data_dir(data_dir_for_child)
                .build(),
        )
        .args(["doctor", "--close-abandoned-runs", "--dry-run"])
        .output()
        .await
    });
    let out = child.await.expect("join").expect("run bookrack doctor");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let _ = shutdown_tx.send(());
    let repl_handle = tokio::task::spawn_blocking(|| -> eyre::Result<()> { Ok(()) });
    runtime
        .run_until_shutdown(None, repl_handle)
        .await
        .expect("daemon stops");

    assert_eq!(
        out.status.code(),
        Some(2),
        "a live daemon refuses the repair as operator input: {text}"
    );
    assert!(
        text.contains("a daemon is serving this library"),
        "the refusal says a daemon is serving the library: {text}"
    );
}
