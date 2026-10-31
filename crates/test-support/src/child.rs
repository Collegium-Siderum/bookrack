// SPDX-License-Identifier: Apache-2.0

//! Process plumbing for tests that drive a `bookrack` child.
//!
//! Spawning the daemon as a child with piped stdout/stderr and then
//! blocking on `wait()` deadlocks the moment either pipe's ~64 KiB
//! kernel buffer fills, because the child blocks on its own write and
//! the test never reads. Tracing-heavy startup paths can saturate the
//! stderr pipe in well under a second of real work.
//!
//! [`DaemonProcess`] fixes this in one place: it spawns the child in a
//! process group of its own, and starts one [`Capture`] per stream
//! that continuously drains it into a shared buffer. The buffer is
//! readable at any moment, so a run that ends badly still reports what
//! the child said: [`DaemonProcess::exit_within`] kills a child that
//! outlives its deadline, and it returns `Ok` only when both streams
//! reached EOF without a read error. A stream a grandchild keeps open,
//! or a read that failed part way, is a [`Failure`] carrying the
//! partial output, never a truncated `Ok` an absence assertion could
//! pass on.
//!
//! Every way a run ends sends `SIGKILL` to the child's whole process
//! group, so whatever the child started goes with it. The leader is
//! watched with `waitid(WNOWAIT)` and reaped only as the last step:
//! until then it holds its pid, which is also the group id, so a signal
//! sent to the group cannot reach a stranger. A leader still running
//! when the run is given up on has its group killed before the streams
//! are drained, so the drain does not wait on a stream a descendant
//! holds; a leader that exited on its own has its streams drained
//! first, keeping what a descendant still prints, and its group killed
//! after. A [`DaemonProcess`] dropped before the leader was reaped —
//! a cancelled wait, a panicking test — kills the group the same way.
//! A descendant that moved to a group or session of its own is outside
//! this reach.
//!
//! [`DaemonProcess::probe_ready`] reads the same stdout buffer for the
//! daemon's ready line, then checks that the session lock is the
//! child's own, that the control socket it names is the one beside the
//! lock, and that the socket answers `daemon.version` on a connection
//! the probe opens and closes itself. It never kills and never consumes
//! the harness, so a caller may cancel it and decide what comes next;
//! [`DaemonProcess::give_up`] ends a run that is not ready by what the
//! leader is doing at that moment, and [`DaemonProcess::wait_ready`] is
//! the two together. Every [`Failure`] names its [`FailureKind`] and
//! the [`Stage`] it happened at, decided by the cause that ended the
//! wait; whatever else the teardown ran into is appended to its reason.
//!
//! Isolation is not this module's job: the caller builds the command
//! through `bookrack_test_support::bookrack_cmd!`, which is the only
//! owner of the child's environment.

use std::fmt;
use std::io;
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
#[cfg(unix)]
use serde_json::Value;
use tokio::io::AsyncReadExt;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// How long a stream may take to reach EOF once the child has exited,
/// and how long a killed child may take to be reaped.
// setting: internal -- how long the harness waits on a child's streams and reap, not a value to tune
const SETTLE_DEADLINE: Duration = Duration::from_secs(5);

/// Start of the line `bookrack run` prints once bring-up has returned.
const READY_PREFIX: &str = "bookrack daemon running: ";

/// How often [`DaemonProcess`] looks at the child's stdout and at
/// whether the leader has exited.
// setting: internal -- the harness's readiness poll interval, not a value to tune
const READY_POLL: Duration = Duration::from_millis(20);

/// File name of the control socket the daemon binds in its runtime
/// directory, beside the session lock.
#[cfg(unix)]
const CONTROL_SOCKET_NAME: &str = "control.sock";

/// The most the readiness probe buffers of one control-plane frame
/// before it has seen the frame's newline.
#[cfg(unix)]
// setting: internal -- bounds what a misbehaving daemon can make the probe buffer, not a value to tune
const PROBE_LINE_CAP: usize = 64 * 1024;

/// The one request the readiness probe sends.
#[cfg(unix)]
const VERSION_REQUEST: &[u8] =
    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"daemon.version\",\"params\":null}\n";

/// What kind of failure ended a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// A deadline or budget ran out with the leader still running.
    Timeout,
    /// The child is not a daemon that is ready: it exited first, or a
    /// readiness check did not hold.
    NotReady,
    /// The leader exited, but a stream did not end cleanly.
    CaptureFailed,
    /// Looking at whether the leader had exited failed.
    WaitFailed,
    /// The streams ended cleanly, but killing the group or reaping the
    /// leader failed.
    ReclaimFailed,
}

/// Where a run was when it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Waiting for the leader, or the leader itself.
    Run,
    /// Handing a stream's bytes to where they are kept.
    Capture,
    /// Waiting for a stream to reach EOF after the leader exited.
    Drain,
    /// Waiting for the ready line.
    ReadyLine,
    /// Reading the session lock.
    DaemonLock,
    /// Checking the control socket the lock names.
    Endpoint,
    /// Calling the control socket.
    ControlPlane,
    /// Waiting for a leader asked to shut down.
    Stop,
}

/// A child that exited on its own, with both streams drained to EOF.
pub struct Exit {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// A run that did not end cleanly: the child outlived its deadline,
/// was not ready, could not be waited on, a stream did not reach EOF
/// without a read error, or the child's group could not be reclaimed.
/// `stdout` and `stderr` hold whatever was read before the run was
/// given up on.
pub struct Failure {
    pub kind: FailureKind,
    pub stage: Stage,
    /// The leader's own status when it exited by itself; `None` when it
    /// was killed or could not be reaped.
    pub exit: Option<ExitStatus>,
    /// The cause that ended the run, then every later problem, joined
    /// by `"; "`.
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

/// A daemon that passed every readiness check.
#[derive(Debug, Clone)]
pub struct Ready {
    pub pid: u32,
    /// The ready line, without its newline.
    pub ready_line: String,
    /// The control socket that answered.
    pub control_sock: PathBuf,
}

/// Why [`DaemonProcess::probe_ready`] did not find the daemon ready.
///
/// The fields are public so a caller that cancelled the probe, and so
/// has no value from it, can build one to hand
/// [`DaemonProcess::give_up`].
#[derive(Debug, Clone)]
pub struct ReadyFailure {
    pub kind: FailureKind,
    pub stage: Stage,
    /// This step's own error. Empty when the failure is the leader's
    /// exit itself.
    pub why: String,
}

impl ReadyFailure {
    fn new(kind: FailureKind, stage: Stage, why: String) -> Self {
        Self { kind, stage, why }
    }

    /// The leader exited; `why` is the step error that led to the look,
    /// if any.
    fn exited(why: String) -> Self {
        Self::new(FailureKind::NotReady, Stage::Run, why)
    }

    fn not_ready(stage: Stage, why: String) -> Self {
        Self::new(FailureKind::NotReady, stage, why)
    }
}

/// Everything a drainer task has read so far, plus the read error
/// that ended it early, if any.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    error: Option<io::Error>,
}

/// One stream of the child: a background task reading it in chunks
/// into a buffer the owner can read at any time.
struct Capture {
    state: Arc<Mutex<Captured>>,
    /// `None` once [`Capture::finish`] has run.
    task: Option<JoinHandle<()>>,
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
        Self {
            state,
            task: Some(task),
        }
    }

    /// The first complete line read so far that starts with `prefix`,
    /// without its newline. A line still missing its newline does not
    /// count: a read can end part way through one.
    fn line_starting_with(&self, prefix: &str) -> Option<String> {
        let state = self.state.lock().unwrap();
        state
            .bytes
            .split_inclusive(|byte| *byte == b'\n')
            .filter_map(|line| line.strip_suffix(b"\n"))
            .find(|line| line.starts_with(prefix.as_bytes()))
            .map(|line| String::from_utf8_lossy(line).into_owned())
    }

    /// Give the drainer `within` to reach EOF, then stop it either
    /// way. Returns what was read and, unless EOF was reached without
    /// a read error, the reason it was not.
    async fn finish(&mut self, name: &str, within: Duration) -> (String, Option<String>) {
        let problem = match self.task.take() {
            None => None,
            Some(mut task) => match tokio::time::timeout(within, &mut task).await {
                Ok(Ok(())) => None,
                Ok(Err(err)) => Some(format!("the {name} drainer panicked: {err}")),
                Err(_) => {
                    task.abort();
                    Some(format!(
                        "{name} was still open {within:?} after the child exited"
                    ))
                }
            },
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

/// The three calls a teardown makes on the child's group, held as
/// values so a unit test can make any one of them fail. The production
/// set is [`Syscalls::real`].
#[cfg(unix)]
#[derive(Clone)]
struct Syscalls {
    /// Whether the leader has exited, without reaping it.
    observe: Arc<dyn Fn(Pid) -> io::Result<bool> + Send + Sync>,
    /// Send `SIGKILL` to every member of the group.
    kill_group: Arc<dyn Fn(Pid) -> io::Result<()> + Send + Sync>,
    /// Runs right before the leader is reaped.
    before_reap: Arc<dyn Fn() -> io::Result<()> + Send + Sync>,
}

#[cfg(unix)]
impl Syscalls {
    fn real() -> Self {
        Self {
            observe: Arc::new(|pid| {
                let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
                Ok(rustix::process::waitid(WaitId::Pid(pid), options)?.is_some())
            }),
            kill_group: Arc::new(|pgid| {
                Ok(rustix::process::kill_process_group(pgid, Signal::KILL)?)
            }),
            before_reap: Arc::new(|| Ok(())),
        }
    }
}

/// Kills the child's group when a [`DaemonProcess`] is dropped before
/// its leader was reaped. The signal is sent, not waited on.
struct GroupGuard {
    #[cfg(unix)]
    pgid: Pid,
    #[cfg(unix)]
    kill_group: Arc<dyn Fn(Pid) -> io::Result<()> + Send + Sync>,
    reaped: bool,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.reaped {
            let _ = (self.kill_group)(self.pgid);
        }
    }
}

/// One step of a teardown that did not go cleanly, in the order the
/// teardown ran into it.
enum Problem {
    /// Looking at whether the leader had exited failed.
    Observe(String),
    /// A stream did not reach EOF, or a read from it failed.
    Drain(String),
    /// Killing the group or reaping the leader failed.
    Reclaim(String),
}

impl Problem {
    fn text(&self) -> &str {
        match self {
            Problem::Observe(text) | Problem::Drain(text) | Problem::Reclaim(text) => text,
        }
    }

    /// The kind and stage of a run whose first problem this is.
    fn class(&self) -> (FailureKind, Stage) {
        match self {
            Problem::Observe(_) => (FailureKind::WaitFailed, Stage::Run),
            Problem::Drain(_) => (FailureKind::CaptureFailed, Stage::Drain),
            Problem::Reclaim(_) => (FailureKind::ReclaimFailed, Stage::Run),
        }
    }
}

/// What a teardown found and left.
struct Teardown {
    /// What the teardown's own look at the leader found; a failed look
    /// counts as not exited.
    exited: bool,
    /// The leader's own status when it exited by itself and was reaped;
    /// `None` when it was killed or could not be reaped.
    exit: Option<ExitStatus>,
    stdout: String,
    stderr: String,
    problems: Vec<Problem>,
}

impl Teardown {
    /// A [`Failure`] of `kind` at `stage`, whose reason starts with
    /// `cause` and goes on with every problem the teardown ran into.
    fn failure(self, kind: FailureKind, stage: Stage, cause: String) -> Failure {
        let mut why = cause;
        for problem in &self.problems {
            if !why.is_empty() {
                why.push_str("; ");
            }
            why.push_str(problem.text());
        }
        if why.is_empty() {
            why.push_str("the run was given up on");
        }
        Failure {
            kind,
            stage,
            exit: self.exit,
            why,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }

    /// The run of a leader that exited on its own: an [`Exit`] when the
    /// teardown went cleanly, otherwise a [`Failure`] its first problem
    /// classifies.
    fn judged(self) -> Result<Exit, Failure> {
        match (self.exit, self.problems.first()) {
            (Some(status), None) => Ok(Exit {
                status,
                stdout: self.stdout,
                stderr: self.stderr,
            }),
            (_, Some(first)) => {
                let (kind, stage) = first.class();
                Err(self.failure(kind, stage, String::new()))
            }
            (None, None) => Err(self.failure(
                FailureKind::ReclaimFailed,
                Stage::Run,
                "the child was killed before it was seen to exit".to_string(),
            )),
        }
    }
}

/// Owns a spawned `bookrack run` child plus the capture of each of
/// its output streams.
pub struct DaemonProcess {
    /// Declared before `child`: fields drop in declaration order, so the
    /// group is killed before `kill_on_drop` acts on the leader alone.
    guard: GroupGuard,
    child: Child,
    pid: u32,
    stdout: Capture,
    stderr: Capture,
    #[cfg(unix)]
    syscalls: Syscalls,
}

impl DaemonProcess {
    /// Spawn `cmd` with stdin closed and stdout/stderr piped, and
    /// start draining both streams at once so the child's pipes never
    /// fill while the test is busy elsewhere.
    pub fn spawn(cmd: Command) -> io::Result<Self> {
        Self::spawn_with_stdin(cmd, Stdio::null())
    }

    /// [`spawn`](Self::spawn) with `stdin` as the child's standard
    /// input.
    pub fn spawn_with_stdin(cmd: Command, stdin: Stdio) -> io::Result<Self> {
        Self::spawn_inner(
            cmd,
            stdin,
            #[cfg(unix)]
            Syscalls::real(),
        )
    }

    fn spawn_inner(
        mut cmd: Command,
        stdin: Stdio,
        #[cfg(unix)] syscalls: Syscalls,
    ) -> io::Result<Self> {
        cmd.stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("the spawned child has no pid"))?;
        #[cfg(unix)]
        let pgid = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::other(format!("the child's pid {pid} is not a valid pid")))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        Ok(Self {
            guard: GroupGuard {
                #[cfg(unix)]
                pgid,
                #[cfg(unix)]
                kill_group: Arc::clone(&syscalls.kill_group),
                reaped: false,
            },
            child,
            pid,
            stdout: Capture::start(stdout),
            stderr: Capture::start(stderr),
            #[cfg(unix)]
            syscalls,
        })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// The daemon's ready line, once stdout holds it whole, without its
    /// newline.
    pub fn ready_line(&self) -> Option<String> {
        self.stdout.line_starting_with(READY_PREFIX)
    }

    /// Wait up to `within` for the daemon to be ready, and hand it
    /// back: [`probe_ready`](Self::probe_ready), then
    /// [`give_up`](Self::give_up) if it fails. The child's group is
    /// killed on every failure path.
    pub async fn wait_ready(mut self, lock_path: &Path, within: Duration) -> Result<Self, Failure> {
        match self.probe_ready(lock_path, within).await {
            Ok(_) => Ok(self),
            Err(failure) => Err(self.give_up(failure).await),
        }
    }

    /// Judge, within `within`, whether the child is a daemon that is
    /// ready. Each round checks, in order:
    ///
    /// 1. the leader has not exited — checked before the ready line, so
    ///    a child that prints the line and exits in one round is not
    ///    passed;
    /// 2. stdout holds a complete ready line, which `bookrack run`
    ///    prints only once bring-up has returned; without one the round
    ///    sleeps and repeats until `within` runs out;
    /// 3. the session lock at `lock_path` names the child's pid and a
    ///    control socket, so a client discovering the daemon through it
    ///    reaches this child and not a lock an earlier daemon left;
    /// 4. that socket is the one beside the lock
    ///    ([`endpoint_matches`]); one that is not is never dialed;
    /// 5. the socket answers `daemon.version` inside what is left of
    ///    `within`, on a connection the probe opens and closes itself;
    /// 6. the leader has still not exited after the answer.
    ///
    /// A failure at step 3, 4, or 5 is followed by one more look at the
    /// leader: if it has exited, that is the failure, and the step's
    /// error is kept as its reason.
    ///
    /// The probe never kills and never consumes the harness. Dropped
    /// part way, it leaves the child running and closes its connection
    /// with it; the caller decides whether to probe again,
    /// [`give_up`](Self::give_up), or [`stop`](Self::stop).
    pub async fn probe_ready(
        &mut self,
        lock_path: &Path,
        within: Duration,
    ) -> Result<Ready, ReadyFailure> {
        let deadline = Instant::now() + within;
        let ready_line = loop {
            self.still_running()?;
            if let Some(line) = self.ready_line() {
                break line;
            }
            if Instant::now() >= deadline {
                return Err(ReadyFailure::new(
                    FailureKind::Timeout,
                    Stage::ReadyLine,
                    format!("the child printed no ready line within {within:?}"),
                ));
            }
            tokio::time::sleep(READY_POLL).await;
        };
        let control_sock = match self.check_and_dial(lock_path, within, deadline).await {
            Ok(control_sock) => control_sock,
            Err(failure) => {
                return Err(match self.leader_exited() {
                    Ok(true) => ReadyFailure::exited(failure.why),
                    _ => failure,
                });
            }
        };
        self.still_running()?;
        Ok(Ready {
            pid: self.pid,
            ready_line,
            control_sock,
        })
    }

    /// End a run that is not ready, by what the leader is doing now
    /// rather than when `failure` was found.
    ///
    /// A leader that has exited is the failure: `NotReady` at `Run`,
    /// with its own status, its streams drained before its group is
    /// killed, and a reason that starts with how it exited and keeps
    /// `failure`'s reason as a lead. A leader still running has its
    /// group killed before the drain, and the failure is `failure`'s.
    pub async fn give_up(mut self, failure: ReadyFailure) -> Failure {
        let end = self.teardown().await;
        if end.exited {
            let mut why = match end.exit {
                Some(status) => format!("the child exited with {status} before it was ready"),
                None => "the child exited before it was ready".to_string(),
            };
            if !failure.why.is_empty() {
                why.push_str("; ");
                why.push_str(&failure.why);
            }
            end.failure(FailureKind::NotReady, Stage::Run, why)
        } else {
            end.failure(failure.kind, failure.stage, failure.why)
        }
    }

    /// Wait up to `within` for the child to exit on its own. A child
    /// still running at the deadline is killed and reported as a
    /// [`Failure`]; one that exited is `Ok` only once both streams
    /// reached EOF without a read error and its group was reclaimed,
    /// so a stream a grandchild keeps open is a failure too, not a
    /// truncated success.
    pub async fn exit_within(mut self, within: Duration) -> Result<Exit, Failure> {
        let deadline = Instant::now() + within;
        let cause = loop {
            match self.leader_exited() {
                Ok(true) => break None,
                Ok(false) if Instant::now() >= deadline => {
                    break Some((
                        FailureKind::Timeout,
                        format!("the child did not exit within {within:?}"),
                    ));
                }
                Ok(false) => {}
                Err(err) => {
                    break Some((
                        FailureKind::WaitFailed,
                        format!("waiting for the child failed: {err}"),
                    ));
                }
            }
            tokio::time::sleep(READY_POLL.min(deadline.saturating_duration_since(Instant::now())))
                .await;
        };
        let end = self.teardown().await;
        match cause {
            Some((kind, why)) => Err(end.failure(kind, Stage::Run, why)),
            None => end.judged(),
        }
    }

    /// Send `SIGTERM` and then [`exit_within`](Self::exit_within), with
    /// a failure at [`Stage::Run`] reported at [`Stage::Stop`].
    /// `Child::kill` is `SIGKILL`, which skips the daemon's graceful
    /// path, so the signal is sent to the leader directly.
    pub async fn stop(self, within: Duration) -> Result<Exit, Failure> {
        let mut this = self;
        this.send_term();
        this.exit_within(within).await.map_err(|mut failure| {
            if failure.stage == Stage::Run {
                failure.stage = Stage::Stop;
            }
            failure
        })
    }

    /// Ask the leader to shut down gracefully. Sent to the leader alone:
    /// the daemon's graceful path stops what it started.
    #[cfg(unix)]
    fn send_term(&mut self) {
        let _ = rustix::process::kill_process(self.guard.pgid, Signal::TERM);
    }

    #[cfg(not(unix))]
    fn send_term(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Steps 1 and 6 of [`probe_ready`](Self::probe_ready): a leader
    /// that has exited is the failure itself.
    fn still_running(&mut self) -> Result<(), ReadyFailure> {
        match self.leader_exited() {
            Ok(false) => Ok(()),
            Ok(true) => Err(ReadyFailure::exited(String::new())),
            Err(err) => Err(ReadyFailure::new(
                FailureKind::WaitFailed,
                Stage::Run,
                format!("polling the child failed: {err}"),
            )),
        }
    }

    /// Steps 3 to 5 of [`probe_ready`](Self::probe_ready). Returns the
    /// control socket that answered.
    async fn check_and_dial(
        &self,
        lock_path: &Path,
        within: Duration,
        deadline: Instant,
    ) -> Result<PathBuf, ReadyFailure> {
        let lock_failure = |why| ReadyFailure::not_ready(Stage::DaemonLock, why);
        let sock = match bookrack_session::peek_lock(lock_path) {
            Ok(Some(info)) if info.pid == self.pid => match info.control_sock {
                Some(sock) => sock,
                None => {
                    return Err(lock_failure(format!(
                        "the session lock at {} names no control socket",
                        lock_path.display(),
                    )));
                }
            },
            Ok(Some(info)) => {
                return Err(lock_failure(format!(
                    "the session lock at {} names pid {}, not the child's {}",
                    lock_path.display(),
                    info.pid,
                    self.pid,
                )));
            }
            Ok(None) => {
                return Err(lock_failure(format!(
                    "there is no session lock at {} after the ready line",
                    lock_path.display(),
                )));
            }
            Err(err) => {
                return Err(lock_failure(format!(
                    "reading the session lock failed: {err:#}"
                )));
            }
        };
        dial(lock_path, sock, within, deadline).await
    }

    /// End the run: look afresh at whether the leader has exited, kill
    /// its group, stop both captures, and reap the leader last.
    ///
    /// A leader still running has its group killed before the drain,
    /// so the drain does not wait on a stream a descendant holds; one
    /// that exited on its own is drained first and its group killed
    /// after, so what its descendants still print is kept. A failed
    /// look counts as a running leader: killing too much is the safe
    /// side.
    async fn teardown(&mut self) -> Teardown {
        let mut problems = Vec::new();
        let exited = match self.leader_exited() {
            Ok(exited) => exited,
            Err(err) => {
                problems.push(Problem::Observe(format!(
                    "observing the child failed: {err}"
                )));
                false
            }
        };
        let (stdout, stderr) = if exited {
            let drained = self.drain(&mut problems).await;
            self.kill_group(&mut problems);
            drained
        } else {
            self.kill_group(&mut problems);
            self.drain(&mut problems).await
        };
        let status = self.reap(&mut problems).await;
        Teardown {
            exited,
            exit: status.filter(|_| exited),
            stdout,
            stderr,
            problems,
        }
    }

    /// Whether the leader has exited, without reaping it, so its pid
    /// and group id stay reserved.
    #[cfg(unix)]
    fn leader_exited(&self) -> io::Result<bool> {
        loop {
            match (self.syscalls.observe)(self.guard.pgid) {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }

    #[cfg(not(unix))]
    fn leader_exited(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    /// Send `SIGKILL` to the group. A group with no member left to
    /// signal is not a failure.
    #[cfg(unix)]
    fn kill_group(&self, problems: &mut Vec<Problem>) {
        match (self.syscalls.kill_group)(self.guard.pgid) {
            Ok(()) => {}
            Err(err) if self.nothing_left_to_signal(&err) => {}
            Err(err) => problems.push(Problem::Reclaim(format!("killing the group failed: {err}"))),
        }
    }

    #[cfg(not(unix))]
    fn kill_group(&mut self, problems: &mut Vec<Problem>) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Err(err) = self.child.start_kill() {
            problems.push(Problem::Reclaim(format!("killing it failed: {err}")));
        }
    }

    /// Whether a failed group signal only means that no live member was
    /// left. `ESRCH` says so everywhere. XNU skips zombies when it
    /// signals a group and answers `EPERM` when every member it found
    /// was one, so there `EPERM` while the leader has exited, and is a
    /// zombie until it is reaped, says the same.
    #[cfg(unix)]
    fn nothing_left_to_signal(&self, err: &io::Error) -> bool {
        use rustix::io::Errno;
        let errno = err.raw_os_error();
        errno == Some(Errno::SRCH.raw_os_error())
            || (cfg!(target_vendor = "apple")
                && errno == Some(Errno::PERM.raw_os_error())
                && matches!(self.leader_exited(), Ok(true)))
    }

    /// Reap the leader within [`SETTLE_DEADLINE`], disarming the guard
    /// only once it is reaped.
    async fn reap(&mut self, problems: &mut Vec<Problem>) -> Option<ExitStatus> {
        #[cfg(unix)]
        if let Err(err) = (self.syscalls.before_reap)() {
            problems.push(Problem::Reclaim(format!("reaping it failed: {err}")));
            return None;
        }
        match tokio::time::timeout(SETTLE_DEADLINE, self.child.wait()).await {
            Ok(Ok(status)) => {
                self.guard.reaped = true;
                Some(status)
            }
            Ok(Err(err)) => {
                problems.push(Problem::Reclaim(format!("reaping it failed: {err}")));
                None
            }
            Err(_) => {
                problems.push(Problem::Reclaim(format!(
                    "the killed child was not reaped within {SETTLE_DEADLINE:?}"
                )));
                None
            }
        }
    }

    /// Stop both captures at once, giving each [`SETTLE_DEADLINE`] to
    /// reach EOF. Returns what each stream read, and records a problem,
    /// stdout first, for each that did not end cleanly.
    async fn drain(&mut self, problems: &mut Vec<Problem>) -> (String, String) {
        let ((stdout, out_problem), (stderr, err_problem)) = tokio::join!(
            self.stdout.finish("stdout", SETTLE_DEADLINE),
            self.stderr.finish("stderr", SETTLE_DEADLINE),
        );
        problems.extend(
            [out_problem, err_problem]
                .into_iter()
                .flatten()
                .map(Problem::Drain),
        );
        (stdout, stderr)
    }
}

/// Whether `control_sock`, as a session lock at `lock_path` names it,
/// is the control socket a daemon owning that lock binds: all four of
///
/// 1. an absolute path whose components are the root and plain names,
///    with no `..` (`Path::components` already folds an interior `.`
///    away);
/// 2. named `control.sock`;
/// 3. in the lock's own directory, both sides canonicalized, so a
///    directory reached through an alias still matches;
/// 4. an existing socket that is not itself a symbolic link.
///
/// The daemon binds `control.sock` in its runtime directory, writes the
/// lock in the same directory, and records the path as it was given, so
/// a lock naming anything else routes a client somewhere this lock's
/// daemon is not. The check and a later connection are not atomic: it
/// rules out a link already in the tree, not one swapped in alongside.
#[cfg(unix)]
pub fn endpoint_matches(lock_path: &Path, control_sock: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;

    let shown = control_sock.display();
    if !control_sock.is_absolute()
        || control_sock
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(format!(
            "the control socket {shown} is not an absolute path free of `.` and `..`"
        ));
    }
    if control_sock.file_name() != Some(std::ffi::OsStr::new(CONTROL_SOCKET_NAME)) {
        return Err(format!(
            "the control socket {shown} is not named {CONTROL_SOCKET_NAME}"
        ));
    }
    let lock_dir = match lock_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let sock_dir = control_sock.parent().unwrap_or(Path::new("/"));
    let resolve = |dir: &Path| {
        dir.canonicalize()
            .map_err(|err| format!("resolving {} failed: {err}", dir.display()))
    };
    if resolve(sock_dir)? != resolve(lock_dir)? {
        return Err(format!(
            "the control socket {shown} is not in the directory of the session lock at {}",
            lock_path.display(),
        ));
    }
    let meta = std::fs::symlink_metadata(control_sock)
        .map_err(|err| format!("the control socket {shown} cannot be inspected: {err}"))?;
    if meta.file_type().is_symlink() {
        return Err(format!("the control socket {shown} is a symbolic link"));
    }
    if !meta.file_type().is_socket() {
        return Err(format!("the control socket {shown} is not a socket"));
    }
    Ok(())
}

/// Steps 4 and 5 of [`DaemonProcess::probe_ready`]: check the endpoint,
/// then call it inside what is left of the budget.
#[cfg(unix)]
async fn dial(
    lock_path: &Path,
    sock: PathBuf,
    within: Duration,
    deadline: Instant,
) -> Result<PathBuf, ReadyFailure> {
    endpoint_matches(lock_path, &sock)
        .map_err(|why| ReadyFailure::not_ready(Stage::Endpoint, why))?;
    let left = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(left, call_daemon_version(&sock)).await {
        Ok(Ok(())) => Ok(sock),
        Ok(Err(reason)) => Err(ReadyFailure::not_ready(
            Stage::ControlPlane,
            format!(
                "the control socket {} did not answer daemon.version: {reason}",
                sock.display(),
            ),
        )),
        Err(_) => Err(ReadyFailure::new(
            FailureKind::Timeout,
            Stage::ControlPlane,
            format!(
                "the control socket {} did not answer daemon.version within {within:?}",
                sock.display(),
            ),
        )),
    }
}

/// Not supported off unix: the probe fails rather than pass a daemon it
/// could not check.
#[cfg(not(unix))]
async fn dial(
    _lock_path: &Path,
    _sock: PathBuf,
    _within: Duration,
    _deadline: Instant,
) -> Result<PathBuf, ReadyFailure> {
    Err(ReadyFailure::not_ready(
        Stage::Endpoint,
        "checking the control socket is not supported on this platform".to_string(),
    ))
}

/// Send `daemon.version` on a connection of its own and read frames
/// until the answer. The connection lives in this future, so it closes
/// with it on every path, cancellation included.
#[cfg(unix)]
async fn call_daemon_version(sock: &Path) -> Result<(), String> {
    let mut stream = tokio::net::UnixStream::connect(sock)
        .await
        .map_err(|err| format!("connecting failed: {err}"))?;
    stream
        .write_all(VERSION_REQUEST)
        .await
        .map_err(|err| format!("sending the request failed: {err}"))?;
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let frame: Vec<u8> = pending.drain(..=end).collect();
            if read_frame(&frame[..end])? == Frame::Answer {
                return Ok(());
            }
        }
        if pending.len() > PROBE_LINE_CAP {
            return Err(format!(
                "a frame ran past {PROBE_LINE_CAP} bytes without a newline"
            ));
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|err| format!("reading the answer failed: {err}"))?;
        if read == 0 {
            return Err("the connection closed before the answer".to_string());
        }
        pending.extend_from_slice(&chunk[..read]);
    }
}

/// A frame the probe accepts.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum Frame {
    /// A server-initiated event, which carries no `id`.
    Event,
    /// The success answer to the probe's one request.
    Answer,
}

/// Classify one frame, newline stripped. Anything but an event or the
/// success answer to request `1` is a violation: the connection carries
/// no other request.
#[cfg(unix)]
fn read_frame(bytes: &[u8]) -> Result<Frame, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| format!("a frame is not JSON: {err}"))?;
    let Some(object) = value.as_object() else {
        return Err(format!("a frame is not a JSON object: {value}"));
    };
    match object.get("id") {
        None if object.get("method").and_then(Value::as_str) == Some("event") => Ok(Frame::Event),
        None => Err(format!(
            "a frame is neither an answer nor an event: {value}"
        )),
        Some(id) if id.as_u64() != Some(1) => {
            Err(format!("a frame answers id {id}, which was never sent"))
        }
        Some(_) if object.contains_key("result") => Ok(Frame::Answer),
        Some(_) => match object.get("error") {
            Some(error) => Err(format!("the answer is an error: {error}")),
            None => Err(format!("the answer has neither result nor error: {value}")),
        },
    }
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::net::unix::pipe::Receiver;

    use super::*;

    fn sh(script: &str, stdin: Stdio, syscalls: Syscalls) -> DaemonProcess {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        DaemonProcess::spawn_inner(cmd, stdin, syscalls).expect("spawn sh")
    }

    /// Whether `rx` reaches EOF within `within`.
    async fn closes_within(rx: &mut Receiver, within: Duration) -> bool {
        let mut buf = [0u8; 64];
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, rx.read(&mut buf)).await {
                Ok(Ok(0)) => return true,
                Ok(Ok(_)) => {}
                Ok(Err(err)) => panic!("reading the probe failed: {err}"),
                Err(_) => return false,
            }
        }
    }

    /// A failed look at the leader counts as a running leader: the group
    /// is killed before the drain, so the run ends at once instead of
    /// waiting out the streams the live tree holds.
    #[tokio::test]
    async fn a_failing_observation_falls_back_to_killing_the_group() {
        let syscalls = Syscalls {
            observe: Arc::new(|_| Err(io::Error::other("injected observation failure"))),
            ..Syscalls::real()
        };
        let (reader, writer) = std::io::pipe().expect("probe pipe");
        let child = sh(
            "exec 3<&0; sleep 30 & exec sleep 30",
            Stdio::from(writer),
            syscalls,
        );
        let mut probe = Receiver::from_owned_fd(OwnedFd::from(reader)).expect("probe receiver");
        assert!(
            !closes_within(&mut probe, Duration::from_millis(200)).await,
            "premise: the probe is open before the run ends",
        );

        let started = Instant::now();
        let failure = match child.exit_within(Duration::from_secs(10)).await {
            Err(failure) => failure,
            Ok(exit) => panic!(
                "a run whose leader could not be observed was Ok: {:?}",
                exit.status
            ),
        };
        let elapsed = started.elapsed();
        assert!(
            closes_within(&mut probe, Duration::from_secs(2)).await,
            "the tree must be killed: {failure}",
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "the group must be killed before the drain, took {elapsed:?}: {failure}",
        );
        assert!(
            failure.why.contains("injected observation failure"),
            "the failure must carry the observation error: {failure}",
        );
        assert_eq!(
            (failure.kind, failure.stage),
            (FailureKind::WaitFailed, Stage::Run),
            "{failure}"
        );
        assert!(failure.exit.is_none(), "a killed leader has no own status");
    }

    /// A clean exit whose group could not be killed is not a clean exit:
    /// nothing vouches that the group's members were signalled.
    #[tokio::test]
    async fn a_failed_group_kill_is_never_a_clean_exit() {
        let syscalls = Syscalls {
            kill_group: Arc::new(|_| Err(io::Error::other("injected group kill failure"))),
            ..Syscalls::real()
        };
        let child = sh("exit 0", Stdio::null(), syscalls);

        let failure = match child.exit_within(Duration::from_secs(5)).await {
            Err(failure) => failure,
            Ok(exit) => panic!("a run whose group kill failed was Ok: {:?}", exit.status),
        };
        assert!(
            failure
                .why
                .contains("killing the group failed: injected group kill failure"),
            "the failure must name the group kill: {failure}",
        );
        assert_eq!(
            (failure.kind, failure.stage),
            (FailureKind::ReclaimFailed, Stage::Run),
            "{failure}"
        );
    }

    /// A clean exit whose leader could not be reaped is not a clean exit,
    /// and the guard stays armed: dropping the harness signals the group
    /// once more.
    #[tokio::test]
    async fn a_failed_reap_is_never_a_clean_exit() {
        let kills = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&kills);
        let real = Syscalls::real();
        let real_kill = Arc::clone(&real.kill_group);
        let syscalls = Syscalls {
            kill_group: Arc::new(move |pgid| {
                counted.fetch_add(1, Ordering::SeqCst);
                real_kill(pgid)
            }),
            before_reap: Arc::new(|| Err(io::Error::other("injected reap failure"))),
            ..real
        };
        let child = sh("exit 0", Stdio::null(), syscalls);

        let failure = match child.exit_within(Duration::from_secs(5)).await {
            Err(failure) => failure,
            Ok(exit) => panic!(
                "a run whose leader was not reaped was Ok: {:?}",
                exit.status
            ),
        };
        assert!(
            failure.why.contains("injected reap failure"),
            "the failure must carry the reap error: {failure}",
        );
        assert_eq!(
            (failure.kind, failure.stage),
            (FailureKind::ReclaimFailed, Stage::Run),
            "{failure}"
        );
        assert!(failure.exit.is_none(), "an unreaped leader has no status");
        assert_eq!(
            kills.load(Ordering::SeqCst),
            2,
            "the teardown and then the still-armed guard each signal the group",
        );
    }

    /// A failed lock or endpoint check is followed by one more look at
    /// the leader, and a leader seen to have exited there is the
    /// failure, with the check's own error kept as its reason.
    #[tokio::test]
    async fn a_lock_or_endpoint_failure_rechecks_the_leader() {
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let foreign = elsewhere.path().join("control.sock");
        for (lock_pid, expected) in [
            ("1", "names pid 1"),
            ("", "is not in the directory of the session lock"),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let lock = dir.path().join("session.lock");
            let looks = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&looks);
            let syscalls = Syscalls {
                observe: Arc::new(move |_| Ok(counted.fetch_add(1, Ordering::SeqCst) > 0)),
                ..Syscalls::real()
            };
            let script = "printf 'pid=%s\\nmcp=127.0.0.1:1\\ncontrol_sock=%s\\n' \"${3:-$$}\" \"$2\" \
                 > \"$1\"; echo 'bookrack daemon running: ready'; exec sleep 30";
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(script)
                .arg("sh")
                .arg(&lock)
                .arg(&foreign)
                .arg(lock_pid);
            let mut child =
                DaemonProcess::spawn_inner(cmd, Stdio::null(), syscalls).expect("spawn sh");
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.ready_line().is_none() {
                assert!(
                    Instant::now() < deadline,
                    "the script printed no ready line"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            let failure = match child.probe_ready(&lock, Duration::from_secs(5)).await {
                Err(failure) => failure,
                Ok(ready) => panic!("a failed check was read as ready: {ready:?}"),
            };
            assert_eq!(
                (failure.kind, failure.stage),
                (FailureKind::NotReady, Stage::Run),
                "pid {lock_pid:?}: a leader seen to have exited at the recheck is the failure: {failure:?}",
            );
            assert!(
                failure.why.contains(expected),
                "pid {lock_pid:?}: the check's own error must be kept: {failure:?}",
            );
            assert_eq!(
                looks.load(Ordering::SeqCst),
                2,
                "pid {lock_pid:?}: one look before the ready line, one recheck",
            );
        }
    }
}
