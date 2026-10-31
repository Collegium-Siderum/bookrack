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

use std::fmt;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_control_client::{ControlSocket, connect};
#[cfg(unix)]
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use serde_json::Value;
use tokio::io::AsyncReadExt;
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

/// A child that exited on its own, with both streams drained to EOF.
pub struct Exit {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// A run that did not end cleanly: the child outlived its deadline,
/// could not be waited on, a stream did not reach EOF without a read
/// error, or the child's group could not be reclaimed. `stdout` and
/// `stderr` hold whatever was read before the run was given up on.
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

    /// Everything read so far.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.state.lock().unwrap().bytes).into_owned()
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
}

/// What a teardown found and left.
struct Teardown {
    /// The leader's own status when it exited by itself and was reaped;
    /// `None` when it was killed or could not be reaped.
    exit: Option<ExitStatus>,
    stdout: String,
    stderr: String,
    problems: Vec<Problem>,
}

impl Teardown {
    /// A [`Failure`] whose reason starts with `cause` and goes on with
    /// every problem the teardown ran into.
    fn failure(self, cause: String) -> Failure {
        let mut why = cause;
        for problem in &self.problems {
            if !why.is_empty() {
                why.push_str("; ");
            }
            why.push_str(problem.text());
        }
        if why.is_empty() {
            why.push_str("the child was killed before it was seen to exit");
        }
        Failure {
            why,
            stdout: self.stdout,
            stderr: self.stderr,
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
        #[cfg(unix)]
        let pgid = child
            .id()
            .and_then(|id| Pid::from_raw(i32::try_from(id).ok()?))
            .ok_or_else(|| io::Error::other("the spawned child has no pid"))?;
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
            stdout: Capture::start(stdout),
            stderr: Capture::start(stderr),
            #[cfg(unix)]
            syscalls,
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
    /// child printed. The child's group is killed on every failure
    /// path.
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
            match self.leader_exited() {
                Ok(true) => {
                    let end = self.teardown().await;
                    let cause = match end.exit {
                        Some(status) => {
                            format!("the child exited with {status} before it was ready")
                        }
                        None => "the child exited before it was ready".to_string(),
                    };
                    return Err(end.failure(cause));
                }
                Ok(false) => {}
                Err(err) => {
                    return Err(self
                        .terminate(format!("polling the child failed: {err}"))
                        .await);
                }
            }
            if Instant::now() >= deadline {
                return Err(self
                    .terminate(format!("the child printed no ready line within {within:?}"))
                    .await);
            }
            tokio::time::sleep(READY_POLL).await;
        }

        let pid = self.id();
        let info = match bookrack_session::peek_lock(lock_path) {
            Ok(Some(info)) if Some(info.pid) == pid => info,
            Ok(Some(info)) => {
                return Err(self
                    .terminate(format!(
                        "the session lock at {} names pid {}, not the child's {pid:?}",
                        lock_path.display(),
                        info.pid,
                    ))
                    .await);
            }
            Ok(None) => {
                return Err(self
                    .terminate(format!(
                        "there is no session lock at {} after the ready line",
                        lock_path.display(),
                    ))
                    .await);
            }
            Err(err) => {
                return Err(self
                    .terminate(format!("reading the session lock failed: {err:#}"))
                    .await);
            }
        };
        let Some(sock) = info.control_sock else {
            return Err(self
                .terminate(format!(
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
                .terminate(format!(
                    "the control socket {} did not answer daemon.version: {err}",
                    sock.display(),
                ))
                .await),
            Err(_) => Err(self
                .terminate(format!(
                    "the control socket {} did not answer daemon.version within {within:?}",
                    sock.display(),
                ))
                .await),
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
                    break Some(format!("the child did not exit within {within:?}"));
                }
                Ok(false) => {}
                Err(err) => break Some(format!("waiting for the child failed: {err}")),
            }
            tokio::time::sleep(READY_POLL.min(deadline.saturating_duration_since(Instant::now())))
                .await;
        };
        let end = self.teardown().await;
        match (cause, end.exit) {
            (None, Some(status)) if end.problems.is_empty() => Ok(Exit {
                status,
                stdout: end.stdout,
                stderr: end.stderr,
            }),
            (cause, _) => Err(end.failure(cause.unwrap_or_default())),
        }
    }

    /// Send `SIGTERM` and then [`exit_within`](Self::exit_within).
    /// `Child::kill` is `SIGKILL`, which skips the daemon's graceful
    /// path, so the signal is sent to the leader directly.
    pub async fn stop(self, within: Duration) -> Result<Exit, Failure> {
        let mut this = self;
        this.send_term();
        this.exit_within(within).await
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

    /// Tear the run down with `cause` as the reason it failed.
    async fn terminate(&mut self, cause: String) -> Failure {
        self.teardown().await.failure(cause)
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

    #[cfg(not(unix))]
    fn kill_group(&mut self, problems: &mut Vec<Problem>) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Err(err) = self.child.start_kill() {
            problems.push(Problem::Reclaim(format!("killing it failed: {err}")));
        }
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
            kills.load(Ordering::SeqCst),
            2,
            "the teardown and then the still-armed guard each signal the group",
        );
    }
}
