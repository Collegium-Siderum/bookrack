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
//! [`CaptureMode::Memory`] keeps every byte, which is what an assertion
//! on a short run wants. [`CaptureMode::Stream`] hands each stream to a
//! [`Sink`] as it is read and keeps only a bounded tail in memory, so a
//! long run costs a fixed amount however much it prints. Each sink is
//! written by a thread of its own, fed through a bounded queue, so a
//! sink that stops accepting output never stalls the drainer, and the
//! child never blocks on a full pipe. A streamed run is `Ok` only once
//! every byte read was written and flushed.
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

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
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
use tokio::sync::mpsc::error::SendTimeoutError;
use tokio::sync::{mpsc, oneshot};
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

/// The most a streamed capture keeps in memory of one stream: its last
/// bytes.
// setting: internal -- bounds a streamed capture's memory, not a value to tune
const TAIL_CAP: usize = 64 * 1024;

/// The longest line a streamed capture still checks for the ready
/// prefix.
// setting: internal -- bounds the ready-line scan of a streamed capture, not a value to tune
const READY_LINE_CAP: usize = 4 * 1024;

/// How many read chunks may wait for a sink's writer.
// setting: internal -- bounds what a slow sink can hold in memory, not a value to tune
const SINK_QUEUE: usize = 64;

/// How long a read chunk may wait for room in a sink's queue, and how
/// long a teardown waits for a sink's writer to report.
// setting: internal -- how long the harness waits on a sink, not a value to tune
const SINK_DEADLINE: Duration = Duration::from_secs(2);

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
///
/// `stdout` and `stderr` are the kept bytes as lossy text: all of them
/// in memory mode, the tail in stream mode.
pub struct Exit {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub output: Output,
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
    pub output: Output,
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

/// How [`DaemonProcess::spawn_with`] starts the child.
pub struct SpawnOpts {
    pub stdin: Stdio,
    pub capture: CaptureMode,
}

/// Standard input closed, both streams kept in memory: what
/// [`DaemonProcess::spawn`] uses.
impl Default for SpawnOpts {
    fn default() -> Self {
        Self {
            stdin: Stdio::null(),
            capture: CaptureMode::Memory,
        }
    }
}

/// How the child's two output streams are kept.
pub enum CaptureMode {
    /// Every byte in memory, without bound.
    Memory,
    /// Each stream written to its sink as it is read, with only its last
    /// 64 KiB kept in memory.
    Stream { stdout: Sink, stderr: Sink },
}

/// Where a streamed capture writes one stream.
pub enum Sink {
    File(std::fs::File),
    /// This process's own standard output.
    Stdout,
    /// This process's own standard error.
    Stderr,
    Writer(Box<dyn Write + Send>),
}

impl Sink {
    fn into_writer(self) -> Box<dyn Write + Send> {
        match self {
            Sink::File(file) => Box::new(file),
            Sink::Stdout => Box::new(io::stdout()),
            Sink::Stderr => Box::new(io::stderr()),
            Sink::Writer(writer) => writer,
        }
    }
}

/// What a run kept of the child's two streams.
#[derive(Debug)]
pub enum Output {
    /// Every byte each stream carried, as read.
    Memory { stdout: Vec<u8>, stderr: Vec<u8> },
    /// What each stream passed through on its way to its sink.
    Streamed {
        stdout: StreamStats,
        stderr: StreamStats,
    },
}

/// What one streamed capture carried and held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStats {
    /// Bytes read from the pipe; on a clean run, also the bytes the sink
    /// wrote.
    pub len: u64,
    /// The most the tail buffer held at once: the tail plus at most one
    /// read chunk.
    pub retained_peak: usize,
}

/// What a drainer task has kept so far.
struct Captured {
    kept: Kept,
    /// Bytes read from the pipe.
    len: u64,
    /// The read error that ended the drainer early, if any.
    error: Option<io::Error>,
    /// Whether the drainer is waiting to hand a chunk to the sink.
    sending: bool,
    /// Why the drainer stopped handing chunks to the sink, if it did.
    sink_trouble: Option<SinkTrouble>,
}

/// The bytes a capture keeps.
enum Kept {
    /// Memory mode: every byte.
    All(Vec<u8>),
    /// Stream mode: the last [`TAIL_CAP`] bytes, the most ever held at
    /// once, and the scan for the ready line.
    Tail {
        bytes: VecDeque<u8>,
        peak: usize,
        scan: LineScan,
    },
}

/// Why a drainer stopped handing chunks to its sink.
#[derive(Clone, Copy)]
enum SinkTrouble {
    /// A chunk waited [`SINK_DEADLINE`] for room in the queue.
    Stalled,
    /// The writer had already stopped, on a failed write.
    Closed,
}

impl Captured {
    fn new(streamed: bool) -> Self {
        let kept = if streamed {
            Kept::Tail {
                bytes: VecDeque::new(),
                peak: 0,
                scan: LineScan::default(),
            }
        } else {
            Kept::All(Vec::new())
        };
        Self {
            kept,
            len: 0,
            error: None,
            sending: false,
            sink_trouble: None,
        }
    }

    fn keep(&mut self, chunk: &[u8]) {
        self.len += chunk.len() as u64;
        match &mut self.kept {
            Kept::All(bytes) => bytes.extend_from_slice(chunk),
            Kept::Tail { bytes, peak, scan } => {
                bytes.extend(chunk);
                *peak = (*peak).max(bytes.len());
                let excess = bytes.len().saturating_sub(TAIL_CAP);
                bytes.drain(..excess);
                scan.feed(chunk);
            }
        }
    }

    /// The first complete line starting with [`READY_PREFIX`], without
    /// its newline. A line still missing its newline does not count: a
    /// read can end part way through one.
    fn ready_line(&self) -> Option<String> {
        match &self.kept {
            Kept::All(bytes) => bytes
                .split_inclusive(|byte| *byte == b'\n')
                .filter_map(|line| line.strip_suffix(b"\n"))
                .find(|line| line.starts_with(READY_PREFIX.as_bytes()))
                .map(|line| String::from_utf8_lossy(line).into_owned()),
            Kept::Tail { scan, .. } => scan.ready.clone(),
        }
    }
}

/// Finds the ready line in a stream that is not kept whole, as it is
/// read.
#[derive(Default)]
struct LineScan {
    /// The line read so far, up to [`READY_LINE_CAP`] bytes.
    current: Vec<u8>,
    /// Whether the current line ran past [`READY_LINE_CAP`]; such a line
    /// is not checked.
    overlong: bool,
    /// The first complete ready line read.
    ready: Option<String>,
}

impl LineScan {
    fn feed(&mut self, chunk: &[u8]) {
        if self.ready.is_some() {
            return;
        }
        for piece in chunk.split_inclusive(|byte| *byte == b'\n') {
            let (body, complete) = match piece.strip_suffix(b"\n") {
                Some(body) => (body, true),
                None => (piece, false),
            };
            if !self.overlong {
                if self.current.len() + body.len() > READY_LINE_CAP {
                    self.overlong = true;
                    self.current.clear();
                } else {
                    self.current.extend_from_slice(body);
                }
            }
            if complete {
                if !self.overlong && self.current.starts_with(READY_PREFIX.as_bytes()) {
                    self.ready = Some(String::from_utf8_lossy(&self.current).into_owned());
                    return;
                }
                self.current.clear();
                self.overlong = false;
            }
        }
    }
}

/// What a finished capture hands its owner.
struct Finished {
    /// The kept bytes, as lossy text.
    text: String,
    part: Part,
    problems: Vec<Problem>,
}

/// One stream's half of an [`Output`].
enum Part {
    Bytes(Vec<u8>),
    Stats(StreamStats),
}

/// One stream of the child: a background task reading it in chunks,
/// keeping them as its mode says and, in stream mode, queueing them for
/// the thread that writes the sink.
struct Capture {
    state: Arc<Mutex<Captured>>,
    /// `None` once [`Capture::finish`] has run.
    task: Option<JoinHandle<()>>,
    /// Stream mode: the writer's one report, the bytes it wrote or its
    /// first error.
    report: Option<oneshot::Receiver<io::Result<u64>>>,
}

impl Capture {
    fn start<R>(mut reader: R, sink: Option<Sink>) -> Self
    where
        R: AsyncReadExt + Unpin + Send + 'static,
    {
        let state = Arc::new(Mutex::new(Captured::new(sink.is_some())));
        let (mut queue, report) = match sink {
            Some(sink) => {
                let (queue, report) = spawn_writer(sink);
                (Some(queue), Some(report))
            }
            None => (None, None),
        };
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            loop {
                let read = match reader.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(err) => {
                        shared.lock().unwrap().error = Some(err);
                        break;
                    }
                };
                shared.lock().unwrap().keep(&chunk[..read]);
                let Some(sender) = &queue else {
                    continue;
                };
                shared.lock().unwrap().sending = true;
                let sent = sender
                    .send_timeout(chunk[..read].to_vec(), SINK_DEADLINE)
                    .await;
                let mut state = shared.lock().unwrap();
                state.sending = false;
                let trouble = match sent {
                    Ok(()) => continue,
                    Err(SendTimeoutError::Timeout(_)) => SinkTrouble::Stalled,
                    Err(SendTimeoutError::Closed(_)) => SinkTrouble::Closed,
                };
                state.sink_trouble = Some(trouble);
                queue = None;
            }
        });
        Self {
            state,
            task: Some(task),
            report,
        }
    }

    /// The ready line, once the stream holds it whole.
    fn ready_line(&self) -> Option<String> {
        self.state.lock().unwrap().ready_line()
    }

    /// End the capture in two timed steps. First the pipe: give the
    /// drainer `within` to reach EOF, then stop it either way and wait
    /// until it is gone, so its end of the queue is closed. Then, in
    /// stream mode, the sink: give the writer [`SINK_DEADLINE`] to report
    /// that it wrote and flushed every byte read. Returns what is kept
    /// and every problem in the order met.
    async fn finish(&mut self, name: &str, within: Duration) -> Finished {
        let mut problems = Vec::new();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(within, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => problems.push(Problem::Drain(format!(
                    "the {name} drainer panicked: {err}"
                ))),
                Err(_) => {
                    problems.push(if self.state.lock().unwrap().sending {
                        Problem::Capture(format!(
                            "the {name} sink was not keeping up {within:?} after the child exited"
                        ))
                    } else {
                        Problem::Drain(format!(
                            "{name} was still open {within:?} after the child exited"
                        ))
                    });
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        let (len, trouble) = {
            let mut state = self.state.lock().unwrap();
            if let Some(err) = state.error.take() {
                problems.push(Problem::Drain(format!("reading {name} failed: {err}")));
            }
            (state.len, state.sink_trouble)
        };
        if let Some(report) = self.report.take() {
            let problem = match trouble {
                Some(SinkTrouble::Stalled) => Some(format!(
                    "the {name} sink stopped accepting output for {SINK_DEADLINE:?}"
                )),
                Some(SinkTrouble::Closed) | None => {
                    match tokio::time::timeout(SINK_DEADLINE, report).await {
                        Ok(Ok(Ok(written))) if written == len => None,
                        Ok(Ok(Ok(written))) => Some(format!(
                            "the {name} sink wrote {written} of the {len} bytes read"
                        )),
                        Ok(Ok(Err(err))) => {
                            Some(format!("writing {name} to its sink failed: {err}"))
                        }
                        Ok(Err(_)) => {
                            Some(format!("the {name} sink's writer ended without a report"))
                        }
                        Err(_) => Some(format!(
                            "the {name} sink did not finish within {SINK_DEADLINE:?}"
                        )),
                    }
                }
            };
            problems.extend(problem.map(Problem::Capture));
        }
        let mut state = self.state.lock().unwrap();
        let len = state.len;
        let (text, part) = match &mut state.kept {
            Kept::All(bytes) => {
                let bytes = std::mem::take(bytes);
                (
                    String::from_utf8_lossy(&bytes).into_owned(),
                    Part::Bytes(bytes),
                )
            }
            Kept::Tail { bytes, peak, .. } => {
                let tail: Vec<u8> = bytes.iter().copied().collect();
                let stats = StreamStats {
                    len,
                    retained_peak: *peak,
                };
                (
                    String::from_utf8_lossy(&tail).into_owned(),
                    Part::Stats(stats),
                )
            }
        };
        Finished {
            text,
            part,
            problems,
        }
    }
}

/// Start the thread that writes one stream to `sink`, fed through a
/// queue of [`SINK_QUEUE`] chunks. Once the queue closes it flushes the
/// sink and reports the bytes it wrote; a failed write or flush ends it
/// early with that error as the report.
fn spawn_writer(sink: Sink) -> (mpsc::Sender<Vec<u8>>, oneshot::Receiver<io::Result<u64>>) {
    let (queue, mut chunks) = mpsc::channel::<Vec<u8>>(SINK_QUEUE);
    let (report, reported) = oneshot::channel();
    let mut writer = sink.into_writer();
    std::thread::spawn(move || {
        let mut written = 0u64;
        let result = loop {
            match chunks.blocking_recv() {
                Some(chunk) => {
                    if let Err(err) = writer.write_all(&chunk) {
                        break Err(err);
                    }
                    written += chunk.len() as u64;
                }
                None => break writer.flush().map(|()| written),
            }
        };
        drop(chunks);
        let _ = report.send(result);
    });
    (queue, reported)
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
    /// A stream's bytes did not all reach its sink.
    Capture(String),
    /// Killing the group or reaping the leader failed.
    Reclaim(String),
}

impl Problem {
    fn text(&self) -> &str {
        match self {
            Problem::Observe(text)
            | Problem::Drain(text)
            | Problem::Capture(text)
            | Problem::Reclaim(text) => text,
        }
    }

    /// The kind and stage of a run whose first problem this is.
    fn class(&self) -> (FailureKind, Stage) {
        match self {
            Problem::Observe(_) => (FailureKind::WaitFailed, Stage::Run),
            Problem::Drain(_) => (FailureKind::CaptureFailed, Stage::Drain),
            Problem::Capture(_) => (FailureKind::CaptureFailed, Stage::Capture),
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
    output: Output,
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
            output: self.output,
        }
    }

    /// The run of a leader that exited on its own: an [`Exit`] when the
    /// teardown went cleanly, otherwise a [`Failure`] its first problem
    /// classifies.
    // `Failure` travels by value through the public API, as the result of
    // a whole run; boxing it here alone would only add an allocation at a
    // run's end.
    #[allow(clippy::result_large_err)]
    fn judged(self) -> Result<Exit, Failure> {
        match (self.exit, self.problems.first()) {
            (Some(status), None) => Ok(Exit {
                status,
                stdout: self.stdout,
                stderr: self.stderr,
                output: self.output,
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
        Self::spawn_with(cmd, SpawnOpts::default())
    }

    /// [`spawn`](Self::spawn) with the child's standard input and the
    /// way its output is kept taken from `opts`.
    pub fn spawn_with(cmd: Command, opts: SpawnOpts) -> io::Result<Self> {
        Self::spawn_inner(
            cmd,
            opts,
            #[cfg(unix)]
            Syscalls::real(),
        )
    }

    fn spawn_inner(
        mut cmd: Command,
        opts: SpawnOpts,
        #[cfg(unix)] syscalls: Syscalls,
    ) -> io::Result<Self> {
        cmd.stdin(opts.stdin)
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
        let (stdout_sink, stderr_sink) = match opts.capture {
            CaptureMode::Memory => (None, None),
            CaptureMode::Stream { stdout, stderr } => (Some(stdout), Some(stderr)),
        };
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
            stdout: Capture::start(stdout, stdout_sink),
            stderr: Capture::start(stderr, stderr_sink),
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
        self.stdout.ready_line()
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
        let (stdout, stderr, output) = if exited {
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
            output,
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

    /// Finish both captures at once, giving each [`SETTLE_DEADLINE`] to
    /// reach EOF and, in stream mode, [`SINK_DEADLINE`] more for its
    /// sink. Returns what each stream kept, and records each problem,
    /// stdout's first.
    async fn drain(&mut self, problems: &mut Vec<Problem>) -> (String, String, Output) {
        let (stdout, stderr) = tokio::join!(
            self.stdout.finish("stdout", SETTLE_DEADLINE),
            self.stderr.finish("stderr", SETTLE_DEADLINE),
        );
        problems.extend(stdout.problems);
        problems.extend(stderr.problems);
        let output = match (stdout.part, stderr.part) {
            (Part::Bytes(out), Part::Bytes(err)) => Output::Memory {
                stdout: out,
                stderr: err,
            },
            (Part::Stats(out), Part::Stats(err)) => Output::Streamed {
                stdout: out,
                stderr: err,
            },
            _ => unreachable!("both streams of a child share one capture mode"),
        };
        (stdout.text, stderr.text, output)
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
        let opts = SpawnOpts {
            stdin,
            ..SpawnOpts::default()
        };
        DaemonProcess::spawn_inner(cmd, opts, syscalls).expect("spawn sh")
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
                DaemonProcess::spawn_inner(cmd, SpawnOpts::default(), syscalls).expect("spawn sh");
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

    /// A sink that discards everything, for the stream a test is not
    /// about.
    fn discard() -> Sink {
        Sink::Writer(Box::new(io::sink()))
    }

    /// `sh -c script` with `args` as `$1`, `$2`, ..., both streams
    /// streamed.
    fn sh_streamed(
        script: &str,
        args: &[&std::ffi::OsStr],
        stdout: Sink,
        stderr: Sink,
    ) -> DaemonProcess {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script).arg("sh").args(args);
        let opts = SpawnOpts {
            capture: CaptureMode::Stream { stdout, stderr },
            ..SpawnOpts::default()
        };
        DaemonProcess::spawn_with(cmd, opts).expect("spawn sh")
    }

    /// Hands every write to the test as it happens.
    struct Notify(std::sync::mpsc::Sender<Vec<u8>>);

    impl Write for Notify {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let _ = self.0.send(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Blocks in its first write until the test releases it, by sending
    /// or by dropping the sender, which a failing test does on its way
    /// out, so no writer thread is left stuck.
    struct Stuck {
        release: std::sync::mpsc::Receiver<()>,
        released: bool,
    }

    impl Write for Stuck {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.released {
                let _ = self.release.recv();
                self.released = true;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn stuck() -> (Sink, std::sync::mpsc::Sender<()>) {
        let (release, wait) = std::sync::mpsc::channel();
        let sink = Stuck {
            release: wait,
            released: false,
        };
        (Sink::Writer(Box::new(sink)), release)
    }

    /// Fails the writes `fails` picks, and its flush when `flush_fails`.
    struct Faulty {
        fails: fn(&[u8]) -> bool,
        flush_fails: bool,
    }

    impl Write for Faulty {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if (self.fails)(buf) {
                return Err(io::Error::other("injected sink failure"));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.flush_fails {
                return Err(io::Error::other("injected flush failure"));
            }
            Ok(())
        }
    }

    fn faulty(fails: fn(&[u8]) -> bool, flush_fails: bool) -> Sink {
        Sink::Writer(Box::new(Faulty { fails, flush_fails }))
    }

    fn expect_capture_failure(result: Result<Exit, Failure>, what: &str) -> Failure {
        let failure = match result {
            Err(failure) => failure,
            Ok(exit) => panic!("{what} was a clean exit: {:?}", exit.status),
        };
        assert_eq!(
            (failure.kind, failure.stage),
            (FailureKind::CaptureFailed, Stage::Capture),
            "{what}: {failure}"
        );
        assert_eq!(
            failure.exit.and_then(|s| s.code()),
            Some(0),
            "{what}: {failure}"
        );
        failure
    }

    /// A streamed capture writes every byte to its sink and keeps only a
    /// tail: the file is whole, byte for byte, while what stays in
    /// memory is exactly the last `TAIL_CAP` bytes and never more than
    /// one read chunk past it.
    #[tokio::test]
    async fn a_streamed_capture_keeps_only_its_tail() {
        const BODY: u64 = 64 * 1024 * 1024;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stdout");
        let file = std::fs::File::create(&path).expect("create the sink file");
        let child = sh_streamed(
            &format!("printf '\\377\\376'; head -c {BODY} /dev/zero; printf 'tail-end\\n'"),
            &[],
            Sink::File(file),
            discard(),
        );

        let exit = child
            .exit_within(Duration::from_secs(60))
            .await
            .expect("a streamed run that wrote every byte is Ok");
        let total = 2 + BODY + 9;
        let mut written = std::fs::File::open(&path).expect("open the sink file");
        assert_eq!(
            written.metadata().expect("stat").len(),
            total,
            "the file is whole"
        );
        let mut head = [0u8; 2];
        io::Read::read_exact(&mut written, &mut head).expect("read the head");
        assert_eq!(head, [0o377, 0o376], "the file starts with the first bytes");
        let mut left = BODY;
        let mut chunk = vec![0u8; 1 << 20];
        while left > 0 {
            let take = left.min(chunk.len() as u64) as usize;
            io::Read::read_exact(&mut written, &mut chunk[..take]).expect("read the body");
            assert!(
                chunk[..take].iter().all(|byte| *byte == 0),
                "the body is zeros"
            );
            left -= take as u64;
        }
        let mut end = Vec::new();
        io::Read::read_to_end(&mut written, &mut end).expect("read the end");
        assert_eq!(end, b"tail-end\n", "the file ends with the last bytes");

        assert!(
            exit.stdout.ends_with("tail-end\n"),
            "the kept text is the tail"
        );
        assert_eq!(exit.stdout.len(), TAIL_CAP, "the tail is exactly TAIL_CAP");
        match exit.output {
            Output::Streamed { stdout, .. } => {
                assert_eq!(stdout.len, total);
                assert!(
                    stdout.retained_peak <= TAIL_CAP + 8192,
                    "the tail held {} bytes at its peak",
                    stdout.retained_peak
                );
            }
            Output::Memory { .. } => panic!("a streamed run reported memory output"),
        }
    }

    /// The ready line is found in a streamed capture as it is read, past
    /// a line too long to be one.
    #[tokio::test]
    async fn a_ready_line_is_seen_while_streaming() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stdout");
        let file = std::fs::File::create(&path).expect("create the sink file");
        let child = sh_streamed(
            "head -c 1048576 /dev/zero | tr '\\0' x; echo; \
             echo 'bookrack daemon running: streamed'; exec sleep 30",
            &[],
            Sink::File(file),
            discard(),
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        let line = loop {
            if let Some(line) = child.ready_line() {
                break line;
            }
            assert!(Instant::now() < deadline, "no ready line was seen");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(line, "bookrack daemon running: streamed");
        child
            .stop(Duration::from_secs(5))
            .await
            .expect("a streamed child stops");
        let written = std::fs::read(&path).expect("read the sink file");
        assert!(
            written.ends_with(b"\nbookrack daemon running: streamed\n"),
            "the sink got the line too"
        );
    }

    /// A streamed sink gets the child's output while the child runs, not
    /// only once it has exited.
    #[tokio::test]
    async fn a_streamed_sink_receives_bytes_before_the_child_exits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let go = dir.path().join("go");
        let (seen, writes) = std::sync::mpsc::channel();
        let child = sh_streamed(
            "echo first; while [ ! -e \"$1\" ]; do sleep 0.01; done",
            &[go.as_os_str()],
            Sink::Writer(Box::new(Notify(seen))),
            discard(),
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while !got.ends_with(b"first\n") {
            match writes.try_recv() {
                Ok(bytes) => got.extend(bytes),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "the sink has not seen the line while the child runs"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(err) => panic!("the sink's channel closed: {err}"),
            }
        }
        std::fs::write(&go, b"").expect("create the go file");
        child
            .exit_within(Duration::from_secs(5))
            .await
            .expect("the child exits once released");
    }

    /// A sink whose write fails is a capture failure that carries the
    /// sink's own error. The drainer keeps reading after the writer is
    /// gone, so the child never blocks on a full pipe and the run ends at
    /// once.
    #[tokio::test]
    async fn a_failing_sink_is_a_capture_failure() {
        let child = sh_streamed(
            "head -c 1048576 /dev/zero",
            &[],
            faulty(|_| true, false),
            discard(),
        );

        let started = Instant::now();
        let result = child.exit_within(Duration::from_secs(10)).await;
        let elapsed = started.elapsed();
        let failure = expect_capture_failure(result, "a run whose sink failed");
        assert!(
            elapsed < Duration::from_secs(2),
            "the run must not wait on the failed sink, took {elapsed:?}: {failure}"
        );
        assert!(
            failure.why.contains("injected sink failure"),
            "the sink's own error must be carried: {failure}"
        );
    }

    /// A sink that stops accepting output is a capture failure inside the
    /// two deadlines, not a hang: the drainer gives up on the sink and
    /// goes on reading the pipe.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sink_that_stops_reading_is_a_capture_failure() {
        let (sink, release) = stuck();
        let child = sh_streamed("head -c 1048576 /dev/zero", &[], sink, discard());

        let started = Instant::now();
        let budget = SINK_DEADLINE + SETTLE_DEADLINE + Duration::from_secs(1);
        let result = tokio::time::timeout(budget, child.exit_within(Duration::from_secs(10)))
            .await
            .expect("the run must end inside the sink and settle deadlines");
        let elapsed = started.elapsed();
        let failure = expect_capture_failure(result, "a run whose sink stopped");
        assert!(elapsed < budget, "took {elapsed:?}: {failure}");
        let _ = release.send(());
    }

    /// An output far smaller than the queue never makes the drainer wait,
    /// so only the wait for the writer's report sees a stuck sink.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_small_output_into_a_stuck_sink_is_a_capture_failure() {
        let (sink, release) = stuck();
        let child = sh_streamed("echo one", &[], sink, discard());

        let started = Instant::now();
        let budget = SINK_DEADLINE + Duration::from_secs(1);
        let result = tokio::time::timeout(budget, child.exit_within(Duration::from_secs(10)))
            .await
            .expect("the run must end inside the sink deadline");
        let elapsed = started.elapsed();
        let failure = expect_capture_failure(result, "a run whose sink never wrote");
        assert!(elapsed < budget, "took {elapsed:?}: {failure}");
        let _ = release.send(());
    }

    /// A write that fails after every chunk was queued is still a
    /// capture failure: the queue taking a chunk is not the sink writing
    /// it.
    #[tokio::test]
    async fn a_failing_last_write_is_a_capture_failure() {
        let child = sh_streamed(
            "echo one; sleep 0.2; echo two; sleep 0.2; echo last",
            &[],
            faulty(|buf| buf.ends_with(b"last\n"), false),
            discard(),
        );

        let result = child.exit_within(Duration::from_secs(10)).await;
        let failure = expect_capture_failure(result, "a run whose last write failed");
        assert!(failure.why.contains("injected sink failure"), "{failure}");
    }

    /// A sink that takes every write but cannot flush them is a capture
    /// failure.
    #[tokio::test]
    async fn a_failing_flush_is_a_capture_failure() {
        let child = sh_streamed("echo one", &[], faulty(|_| false, true), discard());

        let result = child.exit_within(Duration::from_secs(10)).await;
        let failure = expect_capture_failure(result, "a run whose flush failed");
        assert!(failure.why.contains("injected flush failure"), "{failure}");
    }

    /// Kills the pid a script wrote to a file, for a descendant outside
    /// the child's group that the harness cannot reach.
    struct KillRecorded(std::path::PathBuf);

    impl Drop for KillRecorded {
        fn drop(&mut self) {
            let pid = std::fs::read_to_string(&self.0)
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .and_then(Pid::from_raw);
            if let Some(pid) = pid {
                let _ = rustix::process::kill_process(pid, Signal::KILL);
            }
        }
    }

    /// The cause that ended the wait decides the kind; a problem met
    /// later is appended to the reason only. A descendant in a group of
    /// its own keeps stdout open past the group kill, so the timeout is
    /// followed by an unfinished drain.
    #[tokio::test]
    async fn the_first_cause_decides_the_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("descendant.pid");
        let _cleanup = KillRecorded(pidfile.clone());
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("perl -e 'setpgrp(0, 0); exec @ARGV' sleep 30 & echo $! > \"$1\"; exec sleep 30")
            .arg("sh")
            .arg(&pidfile);
        let child = DaemonProcess::spawn(cmd).expect("spawn sh");

        let failure = match child.exit_within(Duration::from_secs(1)).await {
            Err(failure) => failure,
            Ok(exit) => panic!("a child past its deadline was Ok: {:?}", exit.status),
        };
        assert_eq!(
            (failure.kind, failure.stage),
            (FailureKind::Timeout, Stage::Run),
            "{failure}"
        );
        assert!(
            failure.why.contains("stdout was still open"),
            "premise: the drain met the descendant's stream: {failure}"
        );
    }

    /// Memory mode keeps the raw bytes, invalid UTF-8 included, on a
    /// clean exit and on a failure alike; the text fields are their lossy
    /// form.
    #[tokio::test]
    async fn the_memory_mode_keeps_raw_bytes() {
        let writes = "printf '\\377a\\n'; printf '\\376b\\n' >&2";
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("{writes}; exit 3"));
        let exit = DaemonProcess::spawn(cmd)
            .expect("spawn sh")
            .exit_within(Duration::from_secs(5))
            .await
            .expect("a child that exits with both streams closed is Ok");
        assert_eq!(exit.status.code(), Some(3));
        match &exit.output {
            Output::Memory { stdout, stderr } => {
                assert_eq!(stdout, b"\xffa\n");
                assert_eq!(stderr, b"\xfeb\n");
            }
            Output::Streamed { .. } => panic!("a memory run reported streamed output"),
        }
        assert_eq!(exit.stdout, String::from_utf8_lossy(b"\xffa\n"));
        assert_eq!(exit.stderr, String::from_utf8_lossy(b"\xfeb\n"));

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("{writes}; exec sleep 30"));
        let failure = match DaemonProcess::spawn(cmd)
            .expect("spawn sh")
            .exit_within(Duration::from_secs(1))
            .await
        {
            Err(failure) => failure,
            Ok(exit) => panic!("a child past its deadline was Ok: {:?}", exit.status),
        };
        match &failure.output {
            Output::Memory { stdout, stderr } => {
                assert_eq!(stdout, b"\xffa\n");
                assert_eq!(stderr, b"\xfeb\n");
            }
            Output::Streamed { .. } => panic!("a memory run reported streamed output"),
        }
    }
}
