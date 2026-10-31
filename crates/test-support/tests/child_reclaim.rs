// SPDX-License-Identifier: Apache-2.0

//! Pins that every way a `DaemonProcess` run ends takes the child's
//! process group with it.
//!
//! Each script runs under `sh` with the write end of a probe pipe as its
//! stdin, duplicates it onto fd 3, and starts a background `sleep` that
//! inherits fd 3. The test holds the read end: it reaches EOF only once
//! every process holding the write end is dead, because the kernel
//! closes a process's descriptors when it dies, zombie or not. Waiting
//! for the descendant to vanish from the process table would not do: an
//! orphan is adopted by a PID 1 that may never reap it.

#![cfg(unix)]

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use bookrack_test_support::DaemonProcess;
use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;
use tokio::process::Command;

/// The harness's stream settle deadline, restated: a drain that waits
/// for a stream a live descendant holds ends after this long.
const SETTLE: Duration = Duration::from_secs(5);

/// How long a killed tree may take to close the probe.
const EOF_WITHIN: Duration = Duration::from_secs(2);

/// The read end of the probe pipe, plus the file the script writes its
/// descendant's pid to. Dropped while the probe is still open, it kills
/// that pid, so a run against a harness that leaks the tree leaves no
/// `sleep` behind: a process still holding the write end is alive, and
/// a live process's pid cannot have been reused.
struct Probe {
    rx: Receiver,
    pidfile: PathBuf,
    closed: bool,
    _dir: TempDir,
}

impl Probe {
    /// Spawn `sh -c script` with the probe's write end as stdin and the
    /// pid file's path as `$1`.
    fn spawn(script: &str) -> (DaemonProcess, Probe) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("descendant.pid");
        let (reader, writer) = std::io::pipe().expect("probe pipe");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script).arg("sh").arg(&pidfile);
        let child = DaemonProcess::spawn_with_stdin(cmd, Stdio::from(writer)).expect("spawn sh");
        let rx = Receiver::from_owned_fd(OwnedFd::from(reader)).expect("probe receiver");
        let probe = Probe {
            rx,
            pidfile,
            closed: false,
            _dir: dir,
        };
        (child, probe)
    }

    /// Whether the probe reaches EOF within `within`.
    async fn closes_within(&mut self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut buf = [0u8; 64];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.rx.read(&mut buf)).await {
                Ok(Ok(0)) => {
                    self.closed = true;
                    return true;
                }
                Ok(Ok(_)) => {}
                Ok(Err(err)) => panic!("reading the probe failed: {err}"),
                Err(_) => return false,
            }
        }
    }

    /// The probe is open before the reclaim under test: a probe closed
    /// from the start would let every later assertion pass on nothing.
    async fn assert_open(&mut self) {
        assert!(
            !self.closes_within(Duration::from_millis(200)).await,
            "premise: the probe must be open before the reclaim under test",
        );
    }

    /// The tree is gone: the probe reaches EOF within [`EOF_WITHIN`].
    async fn assert_closed(&mut self, what: &str) {
        assert!(
            self.closes_within(EOF_WITHIN).await,
            "{what}: a descendant still holds the probe {EOF_WITHIN:?} later",
        );
    }

    /// The descendant's pid, once the script has written it.
    async fn descendant(&self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(&self.pidfile)
                && let Some(pid) = text.strip_suffix('\n').and_then(|p| p.parse().ok())
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "the script never wrote its descendant's pid"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let mut buf = [0u8; 1];
        if matches!(self.rx.try_read(&mut buf), Ok(0)) {
            return;
        }
        let pid = std::fs::read_to_string(&self.pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .and_then(Pid::from_raw);
        if let Some(pid) = pid {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
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
    let (child, mut probe) = Probe::spawn(&format!("{TREE_HOLDING_STREAMS}; exec sleep 30"));
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
}

/// A readiness wait that gives up kills the whole group, inside its
/// budget plus the time a kill takes.
#[tokio::test]
async fn a_wait_that_gives_up_takes_its_tree_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let (child, mut probe) = Probe::spawn(&format!("{TREE_HOLDING_STREAMS}; exec sleep 30"));
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
}

/// A child that exits before it is ready, leaving a descendant that
/// closed both streams, still has its group killed: a clean drain is
/// not a reason to leave the tree.
#[tokio::test]
async fn a_child_that_exits_before_ready_takes_its_tree_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let (child, mut probe) = Probe::spawn(&format!("{TREE_HOLDING_PROBE}; exit 1"));
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
}

/// A descendant holding both streams is reported exactly as before, and
/// then killed with the group.
#[tokio::test]
async fn a_held_stream_takes_its_holder_down_with_the_failure() {
    let (child, mut probe) = Probe::spawn(&format!("{TREE_HOLDING_STREAMS}; echo started"));
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
}

/// Two held streams settle together: the drain waits one settle
/// deadline, not one per stream.
#[tokio::test]
async fn a_stream_held_on_both_sides_settles_once() {
    let (child, mut probe) = Probe::spawn(TREE_HOLDING_STREAMS);
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
    let (child, mut probe) = Probe::spawn(&format!("{TREE_HOLDING_PROBE}; exit 0"));
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
    let (child, mut probe) = Probe::spawn(TREE_HOLDING_STREAMS);
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
    let (child, mut probe) = Probe::spawn(&format!("{TREE_HOLDING_STREAMS}; exec sleep 30"));
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
    let (child, mut probe) = Probe::spawn(&format!("{TREE_HOLDING_STREAMS}; exec sleep 30"));
    probe.descendant().await;
    probe.assert_open().await;

    drop(child);
    probe.assert_closed("a dropped child").await;
}
