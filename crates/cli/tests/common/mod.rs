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
//! [`DaemonProcess::wait_ready`] reads the same stdout buffer for the
//! daemon's ready line, then checks that the session lock is the
//! child's own and that its control socket answers. A daemon that died
//! during bring-up, a live one that has not finished it, and a lock an
//! earlier daemon left behind are each a [`Failure`] at the wait,
//! carrying the child's output.
//!
//! Isolation is not this module's job: the caller builds the command
//! through `bookrack_test_support::bookrack_cmd!`, which is the only
//! owner of the child's environment.

#![allow(dead_code)]

use std::fmt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_control_client::{ControlSocket, connect};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// How long a stream may take to reach EOF once the child has exited,
/// and how long a killed child may take to be reaped.
const SETTLE_DEADLINE: Duration = Duration::from_secs(5);

/// Start of the line `bookrack run` prints once bring-up has returned.
const READY_PREFIX: &str = "bookrack daemon running: ";

/// How often [`DaemonProcess::wait_ready`] looks at the child's stdout.
const READY_POLL: Duration = Duration::from_millis(20);

/// Wait until the daemon's TTY session lock contains the MCP address,
/// the marker that startup has finished and the control socket is
/// listening. Returns `true` on success, `false` on timeout.
pub async fn wait_for_lock(path: &Path, timeout: Duration) -> bool {
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

    /// Everything read so far.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.state.lock().unwrap().bytes).into_owned()
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

    /// Wait up to `within` for the daemon to be ready, and hand it
    /// back. Ready is three facts, checked in order:
    ///
    /// 1. the child printed its ready line on stdout, which
    ///    `bookrack run` does only once bring-up has returned;
    /// 2. the session lock at `lock_path` names the child's pid, so a
    ///    client that discovers the daemon through it reaches this
    ///    child and not a lock an earlier daemon left behind;
    /// 3. the control socket that lock names answers `daemon.version`
    ///    inside what is left of `within`.
    ///
    /// A child that exits first, a budget that runs out first, or a
    /// check that does not hold is a [`Failure`] carrying what the
    /// child printed. The child is killed on every failure path.
    pub async fn wait_ready(mut self, lock_path: &Path, within: Duration) -> Result<Self, Failure> {
        let deadline = Instant::now() + within;
        loop {
            if self
                .stdout
                .text()
                .lines()
                .any(|line| line.starts_with(READY_PREFIX))
            {
                break;
            }
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    let (stdout, stderr, problems) = self.drain().await;
                    let mut why = format!("the child exited with {status} before it was ready");
                    for problem in problems {
                        why.push_str("; ");
                        why.push_str(&problem);
                    }
                    return Err(Failure {
                        why,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) => {}
                Err(err) => {
                    return Err(self.kill(format!("polling the child failed: {err}")).await);
                }
            }
            if Instant::now() >= deadline {
                return Err(self
                    .kill(format!("the child printed no ready line within {within:?}"))
                    .await);
            }
            tokio::time::sleep(READY_POLL).await;
        }

        let pid = self.id();
        let info = match bookrack_session::peek_lock(lock_path) {
            Ok(Some(info)) if Some(info.pid) == pid => info,
            Ok(Some(info)) => {
                return Err(self
                    .kill(format!(
                        "the session lock at {} names pid {}, not the child's {pid:?}",
                        lock_path.display(),
                        info.pid,
                    ))
                    .await);
            }
            Ok(None) => {
                return Err(self
                    .kill(format!(
                        "there is no session lock at {} after the ready line",
                        lock_path.display(),
                    ))
                    .await);
            }
            Err(err) => {
                return Err(self
                    .kill(format!("reading the session lock failed: {err:#}"))
                    .await);
            }
        };
        let Some(sock) = info.control_sock else {
            return Err(self
                .kill(format!(
                    "the session lock at {} names no control socket",
                    lock_path.display(),
                ))
                .await);
        };

        let socket = ControlSocket::from_path(&sock);
        let answer =
            tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), async {
                let client = connect(&socket).await?;
                client.call_raw("daemon.version", Value::Null).await
            })
            .await;
        match answer {
            Ok(Ok(_)) => Ok(self),
            Ok(Err(err)) => Err(self
                .kill(format!(
                    "the control socket {} did not answer daemon.version: {err}",
                    sock.display(),
                ))
                .await),
            Err(_) => Err(self
                .kill(format!(
                    "the control socket {} did not answer daemon.version within {within:?}",
                    sock.display(),
                ))
                .await),
        }
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
        let (stdout, stderr, problems) = self.drain().await;
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
        let (stdout, stderr, problems) = self.drain().await;
        for problem in problems {
            why.push_str("; ");
            why.push_str(&problem);
        }
        Failure {
            why,
            stdout,
            stderr,
        }
    }

    /// Stop both captures of a child that is gone, giving each
    /// [`SETTLE_DEADLINE`] to reach EOF. Returns what each stream read
    /// and, for each that did not end cleanly, the reason.
    async fn drain(self) -> (String, String, Vec<String>) {
        let (stdout, out_problem) = self.stdout.finish("stdout", SETTLE_DEADLINE).await;
        let (stderr, err_problem) = self.stderr.finish("stderr", SETTLE_DEADLINE).await;
        let problems = [out_problem, err_problem].into_iter().flatten().collect();
        (stdout, stderr, problems)
    }
}
