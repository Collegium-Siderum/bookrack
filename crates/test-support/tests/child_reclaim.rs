// SPDX-License-Identifier: Apache-2.0

//! Pins that every way a `DaemonProcess` run ends takes the child's
//! process group with it.
//!
//! Each script runs under `sh` with the write end of a probe pipe as its
//! stdin, duplicates it onto fd 3, and starts a background `sleep` that
//! inherits fd 3; the probe reaches EOF once that tree is dead.

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use bookrack_test_support::{DaemonProcess, FailureKind, SpawnOpts, Stage};
use tokio::process::Command;

use common::Probe;

/// The harness's stream settle deadline, restated: a drain that waits
/// for a stream a live descendant holds ends after this long.
const SETTLE: Duration = Duration::from_secs(5);

/// Spawn `sh -c script` with a probe as its stdin and the probe's pid
/// file as `$1`.
fn spawn_tree(script: &str) -> (DaemonProcess, Probe) {
    let (probe, stdin) = Probe::new();
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).arg("sh").arg(probe.pidfile());
    let child = DaemonProcess::spawn_with(
        cmd,
        SpawnOpts {
            stdin,
            ..SpawnOpts::default()
        },
    )
    .expect("spawn sh");
    (child, probe)
}

/// Starts a descendant that holds the probe on fd 3 and inherits
/// stdout and stderr, and records its pid.
const TREE_HOLDING_STREAMS: &str = "exec 3<&0; sleep 30 & echo $! > \"$1\"";

/// Starts a descendant that holds only the probe, and records its pid.
const TREE_HOLDING_PROBE: &str = "exec 3<&0; sleep 30 >/dev/null 2>&1 & echo $! > \"$1\"";

/// A child past its deadline is killed with its whole group, and
/// killing the group first is what lets the drain end at once instead
/// of waiting out a stream the descendant holds.
#[tokio::test]
async fn a_child_past_its_deadline_takes_its_tree_with_it() {
    let (child, mut probe) = spawn_tree(&format!("{TREE_HOLDING_STREAMS}; exec sleep 30"));
    probe.descendant().await;
    probe.assert_open().await;

    let started = Instant::now();
    let failure = match child.exit_within(Duration::from_secs(1)).await {
        Err(failure) => failure,
        Ok(exit) => panic!("a child past its deadline was Ok: {:?}", exit.status),
    };
    let elapsed = started.elapsed();
    probe.assert_closed("a child past its deadline").await;
    assert!(
        elapsed < Duration::from_secs(3),
        "the run must end without waiting out the held streams, took {elapsed:?}: {failure}",
    );
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::Timeout, Stage::Run),
        "{failure}"
    );
    assert!(failure.exit.is_none(), "a killed leader has no own status");
}

/// A readiness wait that gives up kills the whole group, inside its
/// budget plus the time a kill takes.
#[tokio::test]
async fn a_wait_that_gives_up_takes_its_tree_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let (child, mut probe) = spawn_tree(&format!("{TREE_HOLDING_STREAMS}; exec sleep 30"));
    probe.descendant().await;
    probe.assert_open().await;

    let started = Instant::now();
    let failure = match child.wait_ready(&lock, Duration::from_secs(2)).await {
        Err(failure) => failure,
        Ok(_) => panic!("a child that never printed a ready line was read as ready"),
    };
    let elapsed = started.elapsed();
    probe.assert_closed("a wait that gave up").await;
    assert!(
        elapsed < Duration::from_secs(4),
        "the wait must end without waiting out the held streams, took {elapsed:?}: {failure}",
    );
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::Timeout, Stage::ReadyLine),
        "{failure}"
    );
}

/// A child that exits before it is ready, leaving a descendant that
/// closed both streams, still has its group killed: a clean drain is
/// not a reason to leave the tree.
#[tokio::test]
async fn a_child_that_exits_before_ready_takes_its_tree_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let (child, mut probe) = spawn_tree(&format!("{TREE_HOLDING_PROBE}; exit 1"));
    probe.descendant().await;
    probe.assert_open().await;

    let failure = match child.wait_ready(&lock, Duration::from_secs(5)).await {
        Err(failure) => failure,
        Ok(_) => panic!("a child that exited was read as ready"),
    };
    probe
        .assert_closed("a child that exited before ready")
        .await;
    assert!(
        failure.why.starts_with("the child exited"),
        "the failure must say the child exited: {failure}",
    );
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::NotReady, Stage::Run),
        "{failure}"
    );
    assert_eq!(failure.exit.and_then(|s| s.code()), Some(1), "{failure}");
}

/// A descendant holding both streams is reported exactly as before, and
/// then killed with the group.
#[tokio::test]
async fn a_held_stream_takes_its_holder_down_with_the_failure() {
    let (child, mut probe) = spawn_tree(&format!("{TREE_HOLDING_STREAMS}; echo started"));
    probe.descendant().await;
    probe.assert_open().await;

    let failure = match child.exit_within(Duration::from_secs(5)).await {
        Err(failure) => failure,
        Ok(exit) => panic!("an unfinished drain was Ok: {:?}", exit.status),
    };
    probe
        .assert_closed("the holder of an unfinished stream")
        .await;
    assert_eq!(
        failure.why,
        "stdout was still open 5s after the child exited; \
         stderr was still open 5s after the child exited",
    );
    assert_eq!(failure.stdout, "started\n");
    assert_eq!(failure.stderr, "");
    assert_eq!(
        (failure.kind, failure.stage),
        (FailureKind::CaptureFailed, Stage::Drain),
        "{failure}"
    );
    assert_eq!(failure.exit.and_then(|s| s.code()), Some(0), "{failure}");
}

/// Two held streams settle together: the drain waits one settle
/// deadline, not one per stream.
#[tokio::test]
async fn a_stream_held_on_both_sides_settles_once() {
    let (child, mut probe) = spawn_tree(TREE_HOLDING_STREAMS);
    probe.descendant().await;

    let started = Instant::now();
    let failure = match child.exit_within(Duration::from_secs(5)).await {
        Err(failure) => failure,
        Ok(exit) => panic!("an unfinished drain was Ok: {:?}", exit.status),
    };
    let elapsed = started.elapsed();
    probe.assert_closed("the holder of both streams").await;
    assert!(
        elapsed < SETTLE + Duration::from_secs(1),
        "both streams must settle inside one deadline, took {elapsed:?}: {failure}",
    );
}

/// A clean exit still kills what the child left in its group.
#[tokio::test]
async fn a_clean_exit_takes_its_stragglers_with_it() {
    let (child, mut probe) = spawn_tree(&format!("{TREE_HOLDING_PROBE}; exit 0"));
    probe.descendant().await;
    probe.assert_open().await;

    let exit = child
        .exit_within(Duration::from_secs(5))
        .await
        .expect("a child that exits with both streams closed is Ok");
    assert!(exit.status.success(), "{:?}", exit.status);
    probe.assert_closed("a straggler after a clean exit").await;
}

/// A clean exit that leaves nothing behind is `Ok`: a group whose only
/// member is the exited leader has no one left to signal, which is not
/// a failed reclaim, however the kernel words it.
#[tokio::test]
async fn a_clean_exit_that_leaves_only_its_leader_is_ok() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("echo done; exit 0");
    let child = DaemonProcess::spawn(cmd).expect("spawn sh");

    let exit = child
        .exit_within(Duration::from_secs(5))
        .await
        .expect("a child that exits alone with both streams closed is Ok");
    assert!(exit.status.success(), "{:?}", exit.status);
    assert_eq!(exit.stdout, "done\n");
}

/// Cancelling `exit_within` while it waits on a held stream still kills
/// the group: the leader is reaped only as the last step, so the guard
/// is armed for the whole drain.
#[tokio::test]
async fn a_cancelled_exit_within_takes_its_tree_with_it() {
    let (child, mut probe) = spawn_tree(TREE_HOLDING_STREAMS);
    probe.descendant().await;
    probe.assert_open().await;

    let cancelled = tokio::time::timeout(
        Duration::from_millis(1500),
        child.exit_within(Duration::from_secs(10)),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "premise: the drain outlives the cancellation"
    );
    probe.assert_closed("a cancelled exit_within").await;
}

/// Cancelling a readiness wait kills the group.
#[tokio::test]
async fn a_cancelled_wait_ready_takes_its_tree_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let (child, mut probe) = spawn_tree(&format!("{TREE_HOLDING_STREAMS}; exec sleep 30"));
    probe.descendant().await;
    probe.assert_open().await;

    let cancelled = tokio::time::timeout(
        Duration::from_millis(500),
        child.wait_ready(&lock, Duration::from_secs(10)),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "premise: the wait outlives the cancellation"
    );
    probe.assert_closed("a cancelled wait_ready").await;
}

/// Dropping the harness kills the group, not only the leader.
#[tokio::test]
async fn a_dropped_child_takes_its_tree_with_it() {
    let (child, mut probe) = spawn_tree(&format!("{TREE_HOLDING_STREAMS}; exec sleep 30"));
    probe.descendant().await;
    probe.assert_open().await;

    drop(child);
    probe.assert_closed("a dropped child").await;
}
