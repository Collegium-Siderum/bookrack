// SPDX-License-Identifier: Apache-2.0

//! The handoff between a command that starts a background process and
//! the process it starts, shared by the stub and the daemon supervisor.
//!
//! The command records its intent first, under the session lock, so
//! recovery sees the start from that moment on. The background process
//! binds its own endpoint before it registers, then waits for a `go` on
//! its stdin before it starts any service: a command killed part way
//! closes the pipe, and the background process reads EOF and leaves
//! without starting anything. There is never a service running with no
//! record of it.
//!
//! Once running, a background process watches its sandbox and ends
//! unasked when the sandbox has gone its idle limit without a call, or
//! as soon as its instance directory is gone.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant, SystemTime};

use bookrack_core::Problem;
use rustix::process::{Pid, Signal};
use serde::{Deserialize, Serialize};

use super::exec::ExitRecord;
use super::{
    Class, Expect, InstanceKind, Outcome, PAUSE_START_BEFORE_GO, STAGE_HANDOFF, STAGE_INSTANCE,
    Session, ToolPaths, ToolResult, instance_ended, now, read_json, write_json,
};

/// How long each side of the handoff waits for the other: the command
/// for the background process to register, the background process for
/// its `go`, and the command for a background process told to leave.
// setting: internal -- the handoff deadline between the tool and its background process
pub(crate) const HANDOFF_DEADLINE: Duration = Duration::from_secs(10);

/// How often a waiting side looks at the instance's files.
// setting: internal -- the tool's state-file poll interval, not a value to tune
pub(crate) const STATE_POLL: Duration = Duration::from_millis(50);

/// How long a background process lets its sandbox go without a call
/// before it ends unasked, unless the start says otherwise.
// setting: internal -- the sandbox tool's default idle limit, overridden by --idle
pub const DEFAULT_IDLE: Duration = Duration::from_secs(15 * 60);

/// How often a running background process looks at its sandbox.
// setting: internal -- the idle watch's poll interval, not a value to tune
pub(crate) const WATCH_POLL: Duration = Duration::from_millis(250);

/// What a background process records once its endpoint is up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub pid: u32,
    pub pgid: u32,
    pub nonce: String,
}

/// Why an instance was closed by someone other than its own process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The start recorded its intent; its background process never
    /// registered, so no service started.
    NeverHandedOff,
    /// A detached daemon's supervisor and everything it held are gone.
    LostSupervisor,
    /// An in-process daemon's holder and everything it held are gone.
    LostHolder,
    /// A stub's process is gone.
    Lost,
    /// The start gave up on the handoff and killed its background
    /// process.
    KilledByStart,
    /// The background process left during the handoff without a record.
    EndedWithoutRecord,
    /// The operator closed the instance with `--forget`.
    Forgotten,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NeverHandedOff => "never-handed-off",
            Reason::LostSupervisor => "lost-supervisor",
            Reason::LostHolder => "lost-holder",
            Reason::Lost => "lost",
            Reason::KilledByStart => "killed-by-start",
            Reason::EndedWithoutRecord => "ended-without-record",
            Reason::Forgotten => "forgotten",
        }
    }
}

/// What an instance closed from outside records, `recovered.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveredRecord {
    pub reason: String,
    pub detail: Option<String>,
    pub at: f64,
}

impl InstanceKind {
    fn name(self) -> &'static str {
        match self {
            InstanceKind::Daemon => "daemon",
            InstanceKind::Stub => "stub",
        }
    }

    /// The hidden subcommand the background process runs.
    fn subcommand(self) -> [&'static str; 2] {
        match self {
            InstanceKind::Daemon => ["daemon", "supervise"],
            InstanceKind::Stub => ["stub", "serve"],
        }
    }

    /// The file the background process registers in.
    pub(crate) fn registration(self) -> &'static str {
        match self {
            InstanceKind::Daemon => "supervisor.json",
            InstanceKind::Stub => "serve.json",
        }
    }

    /// The file the background process's own output goes to.
    fn log(self) -> &'static str {
        match self {
            InstanceKind::Daemon => "supervisor.log",
            InstanceKind::Stub => "stub.log",
        }
    }
}

/// What a running background process makes of its sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Watch {
    /// In use, or not quiet for long enough yet.
    Stay,
    /// No call for the idle limit, none running, and nothing left to
    /// outlast.
    Idle,
    /// The instance's directory is no longer where the process began.
    Removed,
}

impl Watch {
    /// The `cause` an `exit.json` records for an end the watch chose.
    pub(crate) fn cause(self) -> Option<&'static str> {
        match self {
            Watch::Stay => None,
            Watch::Idle => Some("idle"),
            Watch::Removed => Some("removed"),
        }
    }
}

/// Look at the sandbox from the background process of `instance`,
/// which began at `began`.
///
/// An instance directory that is gone is [`Watch::Removed`], whatever
/// the limit. Otherwise, with a `limit`, the sandbox is idle once the
/// later of its last call's end and `began` lies `limit` back, no call
/// holds the session lock, and `outlast` reports nothing left to wait
/// for. The lock and `outlast` are consulted only once the time is up,
/// so a look at a sandbox in use costs one `stat`.
pub(crate) fn watch(
    paths: &ToolPaths,
    instance: &Path,
    began: SystemTime,
    limit: Option<Duration>,
    outlast: impl FnOnce() -> bool,
) -> Watch {
    if let Err(e) = std::fs::symlink_metadata(instance)
        && e.kind() == io::ErrorKind::NotFound
    {
        return Watch::Removed;
    }
    let Some(limit) = limit else {
        return Watch::Stay;
    };
    let last_call = std::fs::metadata(paths.calls().join("last"))
        .and_then(|meta| meta.modified())
        .ok();
    let since = last_call.map_or(began, |at| at.max(began));
    let quiet = SystemTime::now()
        .duration_since(since)
        .unwrap_or(Duration::ZERO);
    if quiet < limit || call_running(paths) || outlast() {
        return Watch::Stay;
    }
    Watch::Idle
}

/// Whether a call holds the sandbox's session lock. A lock file that is
/// not there has no holder; one that cannot be opened or probed counts
/// as held, so a doubt keeps the process.
fn call_running(paths: &ToolPaths) -> bool {
    match std::fs::File::open(paths.lock()) {
        Ok(file) => file.try_lock().is_err(),
        Err(e) => e.kind() != io::ErrorKind::NotFound,
    }
}

/// A background process that has registered and been told to go.
pub(crate) struct Launched {
    pub(crate) child: Child,
    pub(crate) registration: Registration,
}

/// A nonce no other start in this sandbox shares: this process's pid
/// and the time in nanoseconds.
pub(crate) fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// Read a state file the tool owns, checked level by level first.
pub(crate) fn read_state<T: for<'de> Deserialize<'de>>(
    paths: &ToolPaths,
    path: &Path,
) -> Result<T, String> {
    paths.check(path, Expect::File)?;
    read_json(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// [`read_state`] for a file that may not be there yet.
pub(crate) fn read_state_opt<T: for<'de> Deserialize<'de>>(
    paths: &ToolPaths,
    path: &Path,
) -> Result<Option<T>, String> {
    paths.check(path, Expect::FileOrAbsent)?;
    if !path.exists() {
        return Ok(None);
    }
    read_json(path)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Write `recovered.json` for the instance at `instance`.
pub(crate) fn write_recovered(
    instance: &Path,
    reason: Reason,
    detail: Option<String>,
) -> io::Result<RecoveredRecord> {
    let record = RecoveredRecord {
        reason: reason.as_str().to_string(),
        detail,
        at: now(),
    };
    write_json(&instance.join("recovered.json"), &record)?;
    Ok(record)
}

/// Whether `pid` names a live process. One that exists but belongs to
/// someone else is live too.
pub(crate) fn pid_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(err) => err != rustix::io::Errno::SRCH,
    }
}

impl Session {
    /// Step 1 of a start: the next instance number of `kind`, and its
    /// directory, created exclusively.
    pub(crate) fn new_instance(&self, kind: InstanceKind) -> ToolResult<(u32, PathBuf)> {
        let paths = self.paths();
        let dir = paths.instances_dir(kind);
        let refuse =
            |why: String| Outcome::refused(super::STAGE_PATHS, super::problems::path_refused(&why));
        paths.check(&dir, Expect::DirOrAbsent).map_err(refuse)?;
        if !dir.exists() {
            std::fs::create_dir(&dir).map_err(|e| {
                Outcome::io(STAGE_INSTANCE, &format!("create run/{}", kind.name()), e)
            })?;
        }
        let n = self
            .instances(kind)
            .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the instances", e))?
            .last()
            .map_or(1, |n| n + 1);
        let instance = paths.instance(kind, n);
        std::fs::create_dir(&instance)
            .map_err(|e| Outcome::io(STAGE_INSTANCE, "create the instance", e))?;
        Ok((n, instance))
    }

    /// Steps 2 to 4 of a start: start the background process for
    /// instance `n`, wait for it to register, check the registration is
    /// its own, and tell it to go. A failure on the way closes the pipe,
    /// waits for the background process to leave or kills its group,
    /// reaps it, and leaves a `recovered.json` if it left no record.
    pub(crate) async fn hand_off(
        &self,
        kind: InstanceKind,
        n: u32,
        extra: &[OsString],
    ) -> ToolResult<Launched> {
        let paths = self.paths();
        let instance = paths.instance(kind, n);
        let io_err = |what: &str, e: io::Error| Outcome::io(STAGE_HANDOFF, what, e);
        let exe = std::env::current_exe().map_err(|e| io_err("find the tool's own binary", e))?;
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(instance.join(kind.log()))
            .map_err(|e| io_err("create the background process's log", e))?;
        let log_err = log
            .try_clone()
            .map_err(|e| io_err("share the background process's log", e))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(kind.subcommand())
            .arg(paths.dir())
            .arg(n.to_string())
            .args(extra);
        if let Some(point) = &self.opts.pause_at
            && point.starts_with("supervise:")
        {
            cmd.arg("--pause-at").arg(point);
        }
        cmd.process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        let mut child = cmd
            .spawn()
            .map_err(|e| io_err("start the background process", e))?;

        match self.await_registration(kind, &instance, &mut child).await {
            Ok(registration) => Ok(Launched {
                child,
                registration,
            }),
            Err(why) => Err(abandon(&instance, child, why).await),
        }
    }

    /// Wait for the registration and say `go`; the reason on the way out
    /// is why the handoff failed.
    async fn await_registration(
        &self,
        kind: InstanceKind,
        instance: &Path,
        child: &mut Child,
    ) -> Result<Registration, String> {
        let file = instance.join(kind.registration());
        let deadline = Instant::now() + HANDOFF_DEADLINE;
        let registration = loop {
            match read_state_opt::<Registration>(self.paths(), &file) {
                Ok(Some(registration)) => break registration,
                Ok(None) => {}
                Err(why) => return Err(format!("the registration cannot be read: {why}")),
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(format!(
                        "the background process exited with {status} before it registered"
                    ));
                }
                Ok(None) => {}
                Err(e) => return Err(format!("waiting for the background process failed: {e}")),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the background process did not register within {HANDOFF_DEADLINE:?}"
                ));
            }
            tokio::time::sleep(STATE_POLL).await;
        };
        let pid = child.id();
        if registration.pid != pid || registration.pgid != pid {
            return Err(format!(
                "the registration names pid {} in group {}, but the background process is {pid}",
                registration.pid, registration.pgid
            ));
        }
        self.pause(PAUSE_START_BEFORE_GO).await;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "the background process has no stdin".to_string())?;
        stdin
            .write_all(b"go\n")
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("telling the background process to go failed: {e}"))?;
        drop(stdin);
        Ok(registration)
    }
}

/// End a handoff that failed: close the pipe so the background process
/// reads EOF and leaves; wait for that, or kill its group; reap it; and
/// close the instance if the process left no record of its own. The
/// command is the background process's parent until it reaps it, so
/// the pid cannot have been reused when it is signaled.
async fn abandon(instance: &Path, mut child: Child, why: String) -> Outcome {
    drop(child.stdin.take());
    let deadline = Instant::now() + HANDOFF_DEADLINE;
    let mut killed = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => tokio::time::sleep(STATE_POLL).await,
            _ => {
                if let Some(pgid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
                    let _ = rustix::process::kill_process_group(pgid, Signal::KILL);
                }
                let _ = child.wait();
                killed = true;
                break;
            }
        }
    }
    let mut detail = why;
    if !instance_ended(instance) {
        let reason = if killed {
            Reason::KilledByStart
        } else {
            Reason::EndedWithoutRecord
        };
        if let Err(e) = write_recovered(instance, reason, Some(detail.clone())) {
            detail.push_str(&format!("; writing recovered.json failed: {e}"));
        }
    }
    Outcome::refused(
        STAGE_HANDOFF,
        Problem::new("could not hand the start over to its background process").detail(detail),
    )
}

/// What the background process read on its stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GoLine {
    Go,
    Eof,
    Other(String),
    Failed(String),
}

/// Read one line from stdin on a thread of its own, so a caller can
/// race it against a deadline and its requests.
pub(crate) fn read_go() -> tokio::sync::oneshot::Receiver<GoLine> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) => GoLine::Eof,
            Ok(_) if line == "go\n" => GoLine::Go,
            Ok(_) => GoLine::Other(line.trim_end().to_string()),
            Err(e) => GoLine::Failed(e.to_string()),
        };
        let _ = tx.send(read);
    });
    rx
}

/// Why a background process left before its `go`, as its `exit.json`
/// says it.
pub(crate) fn handoff_exit(why: &str) -> ExitRecord {
    let outcome = Outcome {
        class: Class::Refused,
        stage: STAGE_HANDOFF,
        code: None,
        signal: None,
        problem: Some(Problem::new("the start was not handed over").detail(why.to_string())),
    };
    ExitRecord::from(&outcome).with_phase("handoff")
}

/// What the background process makes of its stdin line.
pub(crate) fn go_refusal(line: GoLine) -> Option<String> {
    match line {
        GoLine::Go => None,
        GoLine::Eof => Some("stdin closed before go".to_string()),
        GoLine::Other(text) => Some(format!("stdin carried {text:?}, not go")),
        GoLine::Failed(e) => Some(format!("reading stdin failed: {e}")),
    }
}

/// Register a background process: its pid, its group, and `nonce`.
pub(crate) fn register(instance: &Path, kind: InstanceKind, nonce: &str) -> io::Result<()> {
    let pid = std::process::id();
    let pgid = rustix::process::getpgrp().as_raw_nonzero().get() as u32;
    write_json(
        &instance.join(kind.registration()),
        &Registration {
            pid,
            pgid,
            nonce: nonce.to_string(),
        },
    )
}
