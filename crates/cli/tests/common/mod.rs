// SPDX-License-Identifier: Apache-2.0

//! Process plumbing for the `bookrack run` end-to-end tests.
//!
//! Spawning the daemon as a child with piped stdout/stderr and then
//! blocking on `wait()` deadlocks the moment either pipe's ~64 KiB
//! kernel buffer fills, because the child blocks on its own write and
//! the test never reads. Tracing-heavy startup paths can saturate the
//! stderr pipe in well under a second of real work.
//!
//! [`DaemonProcess`] fixes this in one place: it spawns the child
//! with `kill_on_drop` set so a panicked test never leaks a daemon,
//! and starts one [`Capture`] per stream that continuously drains it
//! into a shared buffer. The buffer is readable at any moment, so a
//! run that ends badly still reports what the child said:
//! [`DaemonProcess::exit_within`] kills a child that outlives its
//! deadline, and it returns `Ok` only when both streams reached EOF
//! without a read error. A stream a grandchild keeps open, or a read
//! that failed part way, is a [`Failure`] carrying the partial output,
//! never a truncated `Ok` an absence assertion could pass on.
//!
//! Isolation is not this module's job: the caller builds the command
//! through `bookrack_test_support::bookrack_cmd!`, which is the only
//! owner of the child's environment.

#![allow(dead_code)]

use std::fmt;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// How long a stream may take to reach EOF once the child has exited,
/// and how long a killed child may take to be reaped.
const SETTLE_DEADLINE: Duration = Duration::from_secs(5);

/// Wait until the daemon's TTY session lock contains the MCP address,
/// the marker that startup has finished and the control socket is
/// listening. Returns `true` on success, `false` on timeout.
pub async fn wait_for_lock(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if text.contains("mcp=") {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// A child that exited on its own, with both streams drained to EOF.
pub struct Exit {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// A run that did not end cleanly: the child outlived its deadline,
/// could not be waited on, or a stream did not reach EOF without a
/// read error. `stdout` and `stderr` hold whatever was read before
/// the run was given up on.
pub struct Failure {
    pub why: String,
    pub stdout: String,
    pub stderr: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.why)?;
        writeln!(f, "--- stdout ---")?;
        writeln!(f, "{}", self.stdout)?;
        writeln!(f, "--- stderr ---")?;
        write!(f, "{}", self.stderr)
    }
}

/// `expect` prints the error through `Debug`; the output is more
/// useful as the same text `Display` renders than as a struct dump
/// with every newline escaped.
impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for Failure {}

/// Everything a drainer task has read so far, plus the read error
/// that ended it early, if any.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    error: Option<std::io::Error>,
}

/// One stream of the child: a background task reading it in chunks
/// into a buffer the owner can read at any time.
struct Capture {
    state: Arc<Mutex<Captured>>,
    task: JoinHandle<()>,
}

impl Capture {
    fn start<R>(mut reader: R) -> Self
    where
        R: AsyncReadExt + Unpin + Send + 'static,
    {
        let state = Arc::new(Mutex::new(Captured::default()));
        let sink = Arc::clone(&state);
        let task = tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => sink.lock().unwrap().bytes.extend_from_slice(&chunk[..n]),
                    Err(err) => {
                        sink.lock().unwrap().error = Some(err);
                        break;
                    }
                }
            }
        });
        Self { state, task }
    }

    /// Give the drainer `within` to reach EOF, then stop it either
    /// way. Returns what was read and, unless EOF was reached without
    /// a read error, the reason it was not.
    async fn finish(mut self, name: &str, within: Duration) -> (String, Option<String>) {
        let problem = match tokio::time::timeout(within, &mut self.task).await {
            Ok(Ok(())) => None,
            Ok(Err(err)) => Some(format!("the {name} drainer panicked: {err}")),
            Err(_) => {
                self.task.abort();
                Some(format!(
                    "{name} was still open {within:?} after the child exited"
                ))
            }
        };
        let mut state = self.state.lock().unwrap();
        let text = String::from_utf8_lossy(&state.bytes).into_owned();
        let problem = match state.error.take() {
            Some(err) => Some(format!("reading {name} failed: {err}")),
            None => problem,
        };
        (text, problem)
    }
}

/// Owns a spawned `bookrack run` child plus the capture of each of
/// its output streams.
pub struct DaemonProcess {
    child: Child,
    stdout: Capture,
    stderr: Capture,
}

impl DaemonProcess {
    /// Spawn `cmd` with stdin closed and stdout/stderr piped, and
    /// start draining both streams at once so the child's pipes never
    /// fill while the test is busy elsewhere.
    pub fn spawn(mut cmd: Command) -> std::io::Result<Self> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        Ok(Self {
            child,
            stdout: Capture::start(stdout),
            stderr: Capture::start(stderr),
        })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Wait up to `within` for the child to exit on its own. A child
    /// still running at the deadline is killed and reported as a
    /// [`Failure`]; one that exited is `Ok` only once both streams
    /// reached EOF without a read error, so a stream a grandchild
    /// keeps open is a failure too, not a truncated success.
    pub async fn exit_within(mut self, within: Duration) -> Result<Exit, Failure> {
        let status = match tokio::time::timeout(within, self.child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => {
                return Err(self
                    .kill(format!("waiting for the child failed: {err}"))
                    .await);
            }
            Err(_) => {
                return Err(self
                    .kill(format!("the child did not exit within {within:?}"))
                    .await);
            }
        };
        let (stdout, out_problem) = self.stdout.finish("stdout", SETTLE_DEADLINE).await;
        let (stderr, err_problem) = self.stderr.finish("stderr", SETTLE_DEADLINE).await;
        let problems: Vec<String> = [out_problem, err_problem].into_iter().flatten().collect();
        if problems.is_empty() {
            Ok(Exit {
                status,
                stdout,
                stderr,
            })
        } else {
            Err(Failure {
                why: problems.join("; "),
                stdout,
                stderr,
            })
        }
    }

    /// Send `SIGTERM` and then [`exit_within`](Self::exit_within).
    /// `Child::kill` is `SIGKILL`, which skips the daemon's graceful
    /// path, so the signal goes through `kill(1)` instead.
    pub async fn stop(self, within: Duration) -> Result<Exit, Failure> {
        if let Some(id) = self.id() {
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(id.to_string())
                .status()
                .await;
        }
        self.exit_within(within).await
    }

    /// Kill the child, reap it, stop both captures, and fold every
    /// step that misbehaves into `why`.
    async fn kill(mut self, mut why: String) -> Failure {
        if let Err(err) = self.child.start_kill() {
            why.push_str(&format!("; killing it failed: {err}"));
        }
        match tokio::time::timeout(SETTLE_DEADLINE, self.child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => why.push_str(&format!("; reaping it failed: {err}")),
            Err(_) => why.push_str(&format!(
                "; the killed child was not reaped within {SETTLE_DEADLINE:?}"
            )),
        }
        let (stdout, out_problem) = self.stdout.finish("stdout", SETTLE_DEADLINE).await;
        let (stderr, err_problem) = self.stderr.finish("stderr", SETTLE_DEADLINE).await;
        for problem in [out_problem, err_problem].into_iter().flatten() {
            why.push_str("; ");
            why.push_str(&problem);
        }
        Failure {
            why,
            stdout,
            stderr,
        }
    }
}
