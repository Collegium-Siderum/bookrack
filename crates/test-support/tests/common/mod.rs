// SPDX-License-Identifier: Apache-2.0

//! A pipe that tells a test when every process holding one end of it
//! is dead.
//!
//! The child gets the write end as its standard input; a script that
//! starts descendants duplicates it onto fd 3 first, so a background
//! job keeps it even where the shell points the job's stdin at
//! `/dev/null`. The test holds the read end, which reaches EOF only
//! once every holder of the write end is dead: the kernel closes a
//! process's descriptors when it dies, zombie or not. Waiting for a
//! descendant to vanish from the process table would not do, because an
//! orphan is adopted by a PID 1 that may never reap it.

// Each test binary that includes this module uses its own subset.
#![allow(dead_code)]

#[cfg(unix)]
pub mod sandbox;

use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;

/// How long a killed tree may take to close the probe.
pub const EOF_WITHIN: Duration = Duration::from_secs(2);

/// The read end of the probe pipe, plus a file a script may write a
/// descendant's pid to. Dropped while the probe is still open, it kills
/// that pid, so a run against a harness that leaks the tree leaves no
/// process behind: a process still holding the write end is alive, and
/// a live process's pid cannot have been reused.
pub struct Probe {
    rx: Receiver,
    dir: TempDir,
    closed: bool,
}

impl Probe {
    /// A new probe, and the write end to hand the child as its stdin.
    pub fn new() -> (Probe, Stdio) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (reader, writer) = std::io::pipe().expect("probe pipe");
        let rx = Receiver::from_owned_fd(OwnedFd::from(reader)).expect("probe receiver");
        let probe = Probe {
            rx,
            dir,
            closed: false,
        };
        (probe, Stdio::from(writer))
    }

    /// Where a script records its descendant's pid.
    pub fn pidfile(&self) -> std::path::PathBuf {
        self.dir.path().join("descendant.pid")
    }

    /// Whether the probe reaches EOF within `within`.
    pub async fn closes_within(&mut self, within: Duration) -> bool {
        if self.closed {
            return true;
        }
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

    /// The probe is open before the step under test: a probe closed
    /// from the start would let every later assertion pass on nothing.
    pub async fn assert_open(&mut self) {
        assert!(
            !self.closes_within(Duration::from_millis(200)).await,
            "premise: the probe must be open before the step under test",
        );
    }

    /// The tree is gone: the probe reaches EOF within [`EOF_WITHIN`].
    pub async fn assert_closed(&mut self, what: &str) {
        assert!(
            self.closes_within(EOF_WITHIN).await,
            "{what}: a process still holds the probe {EOF_WITHIN:?} later",
        );
    }

    /// The descendant's pid, once the script has written it.
    pub async fn descendant(&self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(self.pidfile())
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

    /// Wait until `pid`, a child of this process holding the probe, has
    /// exited and its exit is visible to `waitid`. EOF alone is not
    /// enough: the kernel closes a dying process's descriptors before it
    /// makes the process a zombie. `WNOWAIT` leaves the child unreaped
    /// for its owner.
    pub async fn wait_exit(&mut self, pid: u32) {
        assert!(
            self.closes_within(EOF_WITHIN).await,
            "the child still holds the probe {EOF_WITHIN:?} after it was told to exit",
        );
        let pid = Pid::from_raw(pid as i32).expect("a child's pid is positive");
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if rustix::process::waitid(WaitId::Pid(pid), options)
                .expect("waitid on the child")
                .is_some()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the child's exit is not visible 2s after its probe closed"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
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
        let pid = std::fs::read_to_string(self.pidfile())
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .and_then(Pid::from_raw);
        if let Some(pid) = pid {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
}
