// SPDX-License-Identifier: Apache-2.0

//! The sandbox tool's library side: a persistent sandbox directory, the
//! session lock and call records every command leaves, the in-process
//! [`Session`] that drives a binary under test inside it, and the
//! tripwires that refuse a command before it runs.
//!
//! The tool runs `bookrack` the way the integration suites do, through
//! [`Spawn`], so there is one definition of what an isolated child's
//! environment looks like and no second copy to drift.
//!
//! Layout of a sandbox directory `<dir>`:
//!
//! ```text
//! .session.lock      the session lock; survives `new --replace`
//! .sandbox.json      the marker: tool version, preset, canonical path, ready
//! .calls/<id>/       one result.json per call; .calls/last names the latest
//! run/bin/bookrack   the copied binary under test, run/binary.json its record
//! run/daemon/<n>/    one daemon start: start.json, spawn.json, daemon.json, exit.json, stdout,
//!                    stderr; a detached one also plan.json and its supervisor's files
//! run/stub/<n>/      one detached stub start: start.json, serve.json, url, exit.json
//! run/preset/        a preset build's steps: NN-cmd, NN-stdout, NN-stderr
//! host/              the Sandbox tree the child is confined to
//! golden/            a snapshot of host/, restored in place
//! cells/<id>/        one exec's artifacts; cells/summary.tsv one line per cell
//! ```

pub mod argv;
pub mod build;
mod detach;
pub mod env;
pub mod exec;
pub mod preset;
pub mod recover;
pub mod rich;
pub mod sig;
pub mod snapshot;
pub mod stub;
pub mod supervise;
pub mod tripwire;

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bookrack_config::NO_DOTENV_ENV;
use bookrack_core::Problem;
use serde::{Deserialize, Serialize};

use crate::child::{DaemonProcess, Failure, FailureKind, Stage};
use crate::embed_stub::EmbedStub;
use crate::sandbox::Sandbox;
use crate::spawn::{EnvPlan, Spawn};
use crate::tool::tripwire::Finding;

/// The embedder address a child gets when no stub is running: the
/// discard port, so the built-in `localhost:11434` default never reaches
/// a real Ollama on the host.
pub const NO_STUB_URL: &str = "http://127.0.0.1:9";

/// The tool's own version, written into the marker so a sandbox built
/// by another build of the tool is refused rather than misread.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often a waiting command retries the session lock.
// setting: internal -- the tool's lock poll interval, not a value to tune
const LOCK_POLL: Duration = Duration::from_millis(50);

/// How long a command waits for the session lock unless told otherwise.
// setting: internal -- the tool's default lock wait, overridden by --lock-wait
pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(30);

/// How often a paused command looks for its pause file to be removed.
// setting: internal -- the tool's pause poll interval, not a value to tune
const PAUSE_POLL: Duration = Duration::from_millis(20);

/// Exit code of a command whose child outlived its deadline, after
/// `timeout(1)`.
// setting: internal -- the tool's exit code for a timeout, after timeout(1)
pub const EXIT_TIMEOUT: i32 = 124;

/// Exit code of a refused, not-ready, capture-failed or tool-error
/// command, after `timeout(1)`.
// setting: internal -- the tool's exit code for its own failures, after timeout(1)
pub const EXIT_TOOL: i32 = 125;

/// Base of the exit code for a child a signal terminated.
// setting: internal -- the shell convention for a signal's exit code, not a value to tune
const SIGNAL_EXIT_BASE: i32 = 128;

/// The longest a cell id may be.
// setting: internal -- bounds a cell id's length, not a value to tune
const CELL_ID_MAX: usize = 64;

/// `new --replace` has emptied the old tree and not yet started
/// building the new one.
pub const PAUSE_REPLACE_AFTER_CLEAR: &str = "replace:after-clear";

/// A detached start has recorded its intent and not yet started its
/// background process.
pub const PAUSE_START_AFTER_INTENT: &str = "start:after-intent";

/// A detached start has seen its background process register and not
/// yet told it to go.
pub const PAUSE_START_BEFORE_GO: &str = "start:before-go";

/// A daemon's supervisor has spawned the daemon and not yet recorded
/// its pid. A pause point with this prefix is forwarded to the
/// background process rather than taken by the start itself.
pub const PAUSE_SUPERVISE_AFTER_SPAWN: &str = "supervise:after-spawn";

const LOCK_NAME: &str = ".session.lock";
const MARKER_NAME: &str = ".sandbox.json";
const CALLS_DIR: &str = ".calls";

/// Stage names the tool itself reports; the harness's are
/// [`stage_name`].
pub const STAGE_SESSION_LOCK: &str = "session-lock";
pub const STAGE_PATHS: &str = "paths";
pub const STAGE_INSTANCE: &str = "instance";
pub const STAGE_BINARY: &str = "binary";
pub const STAGE_TRIPWIRE: &str = "tripwire";
pub const STAGE_SPAWN: &str = "spawn";
pub const STAGE_TOOL: &str = "tool";
pub const STAGE_HANDOFF: &str = "handoff";
pub const STAGE_SUPERVISOR: &str = "supervisor";
pub const STAGE_STUB: &str = "stub";
pub const STAGE_PRESET: &str = "preset";

/// Every stage name a record can carry, the harness's and the tool's.
const STAGES: &[&str] = &[
    "run",
    "capture",
    "drain",
    "ready-line",
    "daemon-lock",
    "endpoint",
    "control-plane",
    "stop",
    STAGE_SESSION_LOCK,
    STAGE_PATHS,
    STAGE_INSTANCE,
    STAGE_BINARY,
    STAGE_TRIPWIRE,
    STAGE_SPAWN,
    STAGE_TOOL,
    STAGE_HANDOFF,
    STAGE_SUPERVISOR,
    STAGE_STUB,
    STAGE_PRESET,
];

/// The stage `name` as the tool spells it, or [`STAGE_TOOL`] for a name
/// it does not know.
pub(crate) fn stage_named(name: &str) -> &'static str {
    STAGES
        .iter()
        .copied()
        .find(|stage| *stage == name)
        .unwrap_or(STAGE_TOOL)
}

/// Name the `bookrack` binary under test for the sandbox tool.
///
/// The handle is opaque: the only thing it can be handed to is
/// [`Session::create`], which copies the binary into a sandbox and runs
/// it under the tool's isolation. Like [`bookrack_cmd!`](crate::bookrack_cmd),
/// the lookup lands in the caller's crate, where cargo defines it.
#[macro_export]
macro_rules! bookrack_bin {
    () => {
        $crate::tool::BookrackBin::__new(env!("CARGO_BIN_EXE_bookrack"))
    };
}

/// A `bookrack` binary a test crate built, named through
/// [`bookrack_bin!`] and usable only as a [`Source`].
pub struct BookrackBin {
    path: PathBuf,
}

impl BookrackBin {
    /// Implementation detail of [`bookrack_bin!`]. Not an entry point.
    #[doc(hidden)]
    pub fn __new(path: &str) -> BookrackBin {
        BookrackBin {
            path: PathBuf::from(path),
        }
    }
}

/// Where the binary under test comes from.
pub enum Source {
    /// Built by `cargo build -p bookrack-cli` in the workspace.
    Cargo,
    /// A binary the calling test crate built.
    Prebuilt(BookrackBin),
    /// A script standing in for the binary, for the tool's own tests.
    StandIn(PathBuf),
}

impl Source {
    fn kind(&self) -> &'static str {
        match self {
            Source::Cargo => "cargo",
            Source::Prebuilt(_) => "prebuilt",
            Source::StandIn(_) => "stand-in",
        }
    }
}

/// What a new sandbox is filled with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// One empty library root, `alpha`, registered as the default.
    Bare,
    /// Five libraries in every state a command meets: served ones with
    /// books, papers, an index and none, a failed and a paused job, a
    /// reference book, and two that cannot be mounted (`rich`).
    Rich,
}

impl Preset {
    fn name(self) -> &'static str {
        match self {
            Preset::Bare => "bare",
            Preset::Rich => "rich",
        }
    }
}

/// How a call ended, as the record and the exit code say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    /// The tool's own subcommand succeeded.
    Ok,
    /// The command under test exited by itself.
    Exited,
    /// The command under test was terminated by a signal.
    Signaled,
    /// The command under test outlived its deadline.
    Timeout,
    /// The daemon exited before it was ready, or right after.
    NotReady,
    /// A stream of the command under test did not end cleanly.
    CaptureFailed,
    /// A check refused the command before it ran.
    Refused,
    /// The tool itself failed.
    ToolError,
}

/// The harness's stage names as the record spells them.
pub fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Run => "run",
        Stage::Capture => "capture",
        Stage::Drain => "drain",
        Stage::ReadyLine => "ready-line",
        Stage::DaemonLock => "daemon-lock",
        Stage::Endpoint => "endpoint",
        Stage::ControlPlane => "control-plane",
        Stage::Stop => "stop",
    }
}

/// What one call left: the class, the stage it ended in, the child's
/// exit when it had one, and the three-part problem when it failed.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub class: Class,
    pub stage: &'static str,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub problem: Option<Problem>,
}

/// A result whose error is the [`Outcome`] of a call that did not get
/// to run its command.
pub type ToolResult<T> = Result<T, Outcome>;

impl Outcome {
    pub fn ok() -> Outcome {
        Outcome {
            class: Class::Ok,
            stage: STAGE_TOOL,
            code: None,
            signal: None,
            problem: None,
        }
    }

    pub fn refused(stage: &'static str, problem: Problem) -> Outcome {
        Outcome {
            class: Class::Refused,
            stage,
            code: None,
            signal: None,
            problem: Some(problem),
        }
    }

    pub fn tool_error(stage: &'static str, problem: Problem) -> Outcome {
        Outcome {
            class: Class::ToolError,
            stage,
            code: None,
            signal: None,
            problem: Some(problem),
        }
    }

    /// A tool error from an I/O failure while doing `what`.
    pub fn io(stage: &'static str, what: &str, err: io::Error) -> Outcome {
        Outcome::tool_error(
            stage,
            Problem::new(format!("could not {what}")).detail(err.to_string()),
        )
    }

    /// A command under test that exited by itself.
    pub fn from_exit(status: ExitStatus) -> Outcome {
        use std::os::unix::process::ExitStatusExt;
        match (status.code(), status.signal()) {
            (Some(code), _) => Outcome {
                class: Class::Exited,
                stage: stage_name(Stage::Run),
                code: Some(code),
                signal: None,
                problem: None,
            },
            (None, signal) => Outcome {
                class: Class::Signaled,
                stage: stage_name(Stage::Run),
                code: None,
                signal,
                problem: None,
            },
        }
    }

    /// A run the harness did not end cleanly: the class follows the
    /// failure's kind, the stage is the failure's own, and the child's
    /// exit is kept whenever the harness saw one.
    pub fn from_failure(failure: &Failure) -> Outcome {
        use std::os::unix::process::ExitStatusExt;
        let class = match failure.kind {
            FailureKind::Timeout => Class::Timeout,
            FailureKind::NotReady => Class::NotReady,
            FailureKind::CaptureFailed => Class::CaptureFailed,
            FailureKind::WaitFailed | FailureKind::ReclaimFailed => Class::ToolError,
        };
        let summary = match class {
            Class::Timeout => "the command under test outlived its deadline",
            Class::NotReady => "the daemon under test was not ready",
            Class::CaptureFailed => "a stream of the command under test did not end cleanly",
            _ => "the harness could not observe or reclaim the command under test",
        };
        Outcome {
            class,
            stage: stage_name(failure.stage),
            code: failure.exit.and_then(|s| s.code()),
            signal: failure.exit.and_then(|s| s.signal()),
            problem: Some(Problem::new(summary).detail(failure.why.clone())),
        }
    }

    /// The exit code the tool ends with for this outcome.
    pub fn exit_code(&self) -> i32 {
        match self.class {
            Class::Ok => 0,
            Class::Exited => self.code.unwrap_or(EXIT_TOOL),
            Class::Signaled => SIGNAL_EXIT_BASE + self.signal.unwrap_or(0),
            Class::Timeout => EXIT_TIMEOUT,
            Class::NotReady | Class::CaptureFailed | Class::Refused | Class::ToolError => EXIT_TOOL,
        }
    }

    /// Whether the command under test ran at all.
    pub fn ran(&self) -> bool {
        match self.class {
            Class::Ok | Class::Refused => false,
            Class::ToolError => self.stage == "run" || self.stage == "stop",
            _ => true,
        }
    }

    /// The three parts on stderr: `error:` with the summary, `detail:`
    /// with its lines indented, `hint:` when there is one.
    pub fn eprint(&self) {
        let Some(problem) = &self.problem else { return };
        eprintln!("error: {}", problem.summary);
        if let Some(detail) = &problem.data.detail {
            let mut lines = detail.lines();
            if let Some(first) = lines.next() {
                eprintln!("detail: {first}");
            }
            for line in lines {
                eprintln!("  {line}");
            }
        }
        if let Some(hint) = &problem.data.hint {
            eprintln!("hint: {hint}");
        }
    }
}

/// The three-part problems the tool reports, each built in one place
/// so a summary is worded once.
pub mod problems {
    use std::path::Path;
    use std::time::Duration;

    use bookrack_core::Problem;

    use super::tripwire::Finding;

    pub fn session_lock_timeout(dir: &Path, waited: Duration) -> Problem {
        Problem::new(format!(
            "could not take the session lock of '{}' within {waited:?}",
            dir.display()
        ))
        .detail(format!(
            "another bookrack-sandbox call holds {}",
            dir.join(super::LOCK_NAME).display()
        ))
        .hint("wait for the other bookrack-sandbox call to finish, or raise --lock-wait")
    }

    pub fn not_a_sandbox(dir: &Path, why: &str) -> Problem {
        Problem::new(format!(
            "'{}' is not a sandbox this tool made",
            dir.display()
        ))
        .detail(why.to_string())
        .hint("create one with 'bookrack-sandbox new <dir>'")
    }

    pub fn not_ready(dir: &Path) -> Problem {
        Problem::new(format!("'{}' is only half built", dir.display()))
            .detail("the marker records ready = false: a `new` or `new --replace` did not finish")
            .hint("rebuild it with 'bookrack-sandbox new <dir> --replace'")
    }

    pub fn tripped(findings: &[Finding]) -> Problem {
        let detail = findings
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        Problem::new(format!(
            "cannot run: the isolation check found {} problem{}",
            findings.len(),
            if findings.len() == 1 { "" } else { "s" }
        ))
        .detail(detail)
        .hint(
            "the command's arguments or the sandbox tree point outside the sandbox's host tree; \
             fix the argument, or rebuild the sandbox",
        )
    }

    pub fn binary_changed(why: &str) -> Problem {
        Problem::new("cannot run: the binary under test changed since it was recorded")
            .detail(why.to_string())
            .hint("run 'bookrack-sandbox build <dir>' again")
    }

    pub fn unfinished_instances(instances: &[u32]) -> Problem {
        Problem::new("cannot change the sandbox: a daemon start has not ended")
            .detail(format!(
                "run/daemon/{} ha{} no exit.json",
                instances
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", run/daemon/"),
                if instances.len() == 1 { "s" } else { "ve" }
            ))
            .hint(
                "run 'bookrack-sandbox daemon stop <dir>'; if it cannot tell whether the daemon is \
                 gone, confirm by hand and pass --forget",
            )
    }

    pub fn unfinished_stub(instances: &[u32]) -> Problem {
        Problem::new("cannot change the sandbox: a stub start has not ended")
            .detail(format!(
                "run/stub/{} ha{} no exit.json",
                instances
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", run/stub/"),
                if instances.len() == 1 { "s" } else { "ve" }
            ))
            .hint("run 'bookrack-sandbox teardown <dir>' first")
    }

    pub fn session_lock_held(lock: &Path) -> Problem {
        Problem::new("cannot change the sandbox: a daemon holds its session lock")
            .detail(format!("{} is held", lock.display()))
            .hint("stop the daemon first")
    }

    pub fn session_lock_unknown(lock: &Path, why: &str) -> Problem {
        Problem::new(
            "cannot change the sandbox: whether a daemon holds its session lock cannot be told",
        )
        .detail(format!("{}: {why}", lock.display()))
    }

    pub fn inside_workspace(dir: &Path, workspace: &Path) -> Problem {
        Problem::new("cannot create a sandbox inside the workspace it builds")
            .detail(format!(
                "{} lies under {}",
                dir.display(),
                workspace.display()
            ))
            .hint("choose a directory outside the workspace, on a volume with room for the binary")
    }

    pub fn no_workspace(why: &str) -> Problem {
        Problem::new("cannot tell which workspace to build")
            .detail(why.to_string())
            .hint("pass --workspace <path>")
    }

    pub fn not_empty(dir: &Path, found: &[String]) -> Problem {
        Problem::new(format!("'{}' is not empty", dir.display()))
            .detail(format!("it holds {}", found.join(", ")))
            .hint("pass --replace to rebuild an existing sandbox")
    }

    pub fn bad_cell_id(id: &str) -> Problem {
        Problem::new(format!("'{id}' is not a cell id")).hint(
            "a cell id is 1 to 64 letters, digits, dots, dashes or underscores, starting with a \
             letter or digit",
        )
    }

    pub fn path_refused(why: &str) -> Problem {
        Problem::new("cannot touch the sandbox tree: a path the tool owns is not what it should be")
            .detail(why.to_string())
            .hint("remove the link or stray entry, or rebuild the sandbox with 'new --replace'")
    }

    pub fn leftover_dotenv(path: &Path) -> Problem {
        Problem::new("cannot run: a .env is already in the sandbox's working directory")
            .detail(format!(
                "{} exists; under the session lock only a crashed earlier call can have left it",
                path.display()
            ))
            .hint("inspect it, then remove it by hand")
    }

    pub fn no_golden(dir: &Path) -> Problem {
        Problem::new(format!("'{}' has no golden tree to restore", dir.display()))
            .hint("take one with 'bookrack-sandbox snapshot <dir>'")
    }
}

/// Every path the tool reads or writes under a sandbox, derived from
/// the canonical sandbox directory and never from a caller's string.
#[derive(Debug, Clone)]
pub struct ToolPaths {
    dir: PathBuf,
}

/// The two kinds of background start a sandbox records under `run/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceKind {
    Daemon,
    Stub,
}

/// What a tool-owned path is expected to be when it is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Dir,
    File,
    Absent,
    DirOrAbsent,
    FileOrAbsent,
}

impl ToolPaths {
    /// `dir` must already be canonical.
    pub fn new(dir: PathBuf) -> ToolPaths {
        ToolPaths { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn lock(&self) -> PathBuf {
        self.dir.join(LOCK_NAME)
    }

    pub fn marker(&self) -> PathBuf {
        self.dir.join(MARKER_NAME)
    }

    pub fn calls(&self) -> PathBuf {
        self.dir.join(CALLS_DIR)
    }

    pub fn pause(&self, point: &str) -> PathBuf {
        self.dir.join(format!(".pause-{point}"))
    }

    pub fn run(&self) -> PathBuf {
        self.dir.join("run")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.run().join("bin")
    }

    pub fn bin(&self) -> PathBuf {
        self.bin_dir().join("bookrack")
    }

    pub fn binary_record(&self) -> PathBuf {
        self.run().join("binary.json")
    }

    pub fn daemon_dir(&self) -> PathBuf {
        self.run().join("daemon")
    }

    pub fn daemon_instance(&self, n: u32) -> PathBuf {
        self.daemon_dir().join(n.to_string())
    }

    /// Where a preset's build keeps each step's command and output.
    pub fn preset_dir(&self) -> PathBuf {
        self.run().join("preset")
    }

    pub fn stub_dir(&self) -> PathBuf {
        self.run().join("stub")
    }

    pub fn stub_instance(&self, n: u32) -> PathBuf {
        self.stub_dir().join(n.to_string())
    }

    /// Where the instances of `kind` live.
    pub fn instances_dir(&self, kind: InstanceKind) -> PathBuf {
        match kind {
            InstanceKind::Daemon => self.daemon_dir(),
            InstanceKind::Stub => self.stub_dir(),
        }
    }

    pub fn instance(&self, kind: InstanceKind, n: u32) -> PathBuf {
        self.instances_dir(kind).join(n.to_string())
    }

    pub fn host(&self) -> PathBuf {
        self.dir.join("host")
    }

    pub fn golden(&self) -> PathBuf {
        self.dir.join("golden")
    }

    pub fn cells(&self) -> PathBuf {
        self.dir.join("cells")
    }

    pub fn cell(&self, id: &str) -> PathBuf {
        self.cells().join(id)
    }

    pub fn summary(&self) -> PathBuf {
        self.cells().join("summary.tsv")
    }

    /// Check `target`, which lies under the sandbox directory, level by
    /// level without following links: every directory on the way must
    /// be a real directory, and the target itself must be what `expect`
    /// says. A regular file the tool owns must have exactly one link.
    pub fn check(&self, target: &Path, expect: Expect) -> Result<(), String> {
        let rel = target
            .strip_prefix(&self.dir)
            .map_err(|_| format!("{} is not under {}", target.display(), self.dir.display()))?;
        let mut at = self.dir.clone();
        let parts: Vec<_> = rel.components().collect();
        for (i, part) in parts.iter().enumerate() {
            at.push(part);
            let last = i + 1 == parts.len();
            let meta = match std::fs::symlink_metadata(&at) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    if last
                        && matches!(
                            expect,
                            Expect::Absent | Expect::DirOrAbsent | Expect::FileOrAbsent
                        )
                    {
                        return Ok(());
                    }
                    return Err(format!("{} is missing", at.display()));
                }
                Err(err) => return Err(format!("{} cannot be inspected: {err}", at.display())),
            };
            if meta.file_type().is_symlink() {
                return Err(format!("{} is a symbolic link", at.display()));
            }
            if !last {
                if !meta.is_dir() {
                    return Err(format!("{} is not a directory", at.display()));
                }
                continue;
            }
            match expect {
                Expect::Absent => return Err(format!("{} already exists", at.display())),
                Expect::Dir | Expect::DirOrAbsent => {
                    if !meta.is_dir() {
                        return Err(format!("{} is not a directory", at.display()));
                    }
                }
                Expect::File | Expect::FileOrAbsent => {
                    if !meta.is_file() {
                        return Err(format!("{} is not a regular file", at.display()));
                    }
                    if meta.nlink() > 1 {
                        return Err(format!(
                            "{} has {} links; a file the tool owns must have one",
                            at.display(),
                            meta.nlink()
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// The marker a sandbox directory carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    pub tool_version: String,
    pub preset: String,
    pub source: String,
    pub created: f64,
    /// The canonical path the sandbox was made at; a copy elsewhere does
    /// not match.
    pub dir: String,
    pub workspace: String,
    /// `false` from the start of a `new --replace` until it has finished.
    pub ready: bool,
}

/// One call's record, `.calls/<id>/result.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub call: String,
    pub command: String,
    pub args: Vec<String>,
    pub instance: Option<u32>,
    pub class: Class,
    pub stage: String,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub problem: Option<Problem>,
    pub notes: Vec<String>,
    /// Seconds since the epoch at which the call took the lock, or gave
    /// up waiting for it.
    pub started: f64,
    /// Seconds since the epoch at which the record was written.
    pub finished: f64,
}

/// Seconds since the epoch, as a float.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Write `bytes` to `path` through a temporary file in the same
/// directory and a rename, so a reader sees the old file or the new one
/// and a hard link to the old file is never written through.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("a path to write must have a file name"))?;
    let tmp = path.with_file_name(format!(
        "{}.tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        io::Write::write_all(&mut file, bytes)?;
        io::Write::flush(&mut file)?;
    }
    std::fs::rename(&tmp, path)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// A live daemon a [`Session`] holds.
pub struct LiveDaemon {
    pub instance: u32,
    pub process: DaemonProcess,
    pub control_sock: PathBuf,
}

/// One call in progress: the lock it holds, what it will record.
pub struct Call {
    command: &'static str,
    args: Vec<String>,
    instance: Option<u32>,
    started: f64,
    notes: Vec<String>,
    _lock: File,
    cell: Option<PathBuf>,
}

impl Call {
    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn set_instance(&mut self, n: u32) {
        self.instance = Some(n);
    }

    /// The cell directory this call's record is also written to.
    pub fn set_cell(&mut self, cell: PathBuf) {
        self.cell = Some(cell);
    }
}

/// How a session takes its locks and where it pauses.
#[derive(Debug, Clone)]
pub struct SessionOpts {
    pub lock_wait: Duration,
    pub pause_at: Option<String>,
}

impl Default for SessionOpts {
    fn default() -> Self {
        SessionOpts {
            lock_wait: DEFAULT_LOCK_WAIT,
            pause_at: None,
        }
    }
}

/// What `new` is asked to do beyond building.
#[derive(Debug, Clone, Default)]
pub struct CreateOpts {
    /// Rebuild an existing sandbox in place.
    pub replace: bool,
    /// The workspace to build from, in place of the git toplevel of the
    /// current directory.
    pub workspace: Option<PathBuf>,
}

/// A sandbox opened in this process: one command at a time, each under
/// the session lock, each leaving a record.
pub struct Session {
    paths: ToolPaths,
    opts: SessionOpts,
    stub_url: Option<String>,
    stubs: Vec<EmbedStub>,
    daemon: Option<LiveDaemon>,
}

impl Session {
    /// Build a sandbox at `dir` and open it.
    pub async fn create(
        dir: &Path,
        source: Source,
        preset: Preset,
        create: CreateOpts,
        opts: SessionOpts,
    ) -> ToolResult<Session> {
        let abs = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| Outcome::io(STAGE_PATHS, "read the current directory", e))?
                .join(dir)
        };
        let args: Vec<String> = vec![abs.display().to_string()];

        let existing = if create.replace {
            Some(Self::marker_prelock(&abs)?)
        } else {
            None
        };
        let workspace = Self::resolve_workspace(
            create
                .workspace
                .as_deref()
                .or_else(|| existing.as_ref().map(|(_, m)| Path::new(&m.workspace))),
        )
        .await?;
        if tripwire::is_within(&abs, &workspace).unwrap_or(false) {
            return Err(Outcome::refused(
                STAGE_PATHS,
                problems::inside_workspace(&abs, &workspace),
            ));
        }
        if !create.replace {
            std::fs::create_dir_all(&abs)
                .map_err(|e| Outcome::io(STAGE_PATHS, "create the sandbox directory", e))?;
        }
        let paths = ToolPaths::new(
            abs.canonicalize()
                .map_err(|e| Outcome::io(STAGE_PATHS, "resolve the sandbox directory", e))?,
        );
        let mut session = Session {
            paths,
            opts,
            stub_url: None,
            stubs: Vec::new(),
            daemon: None,
        };
        let lock = match session.acquire(true).await {
            Ok(lock) => lock,
            Err(outcome) => {
                if create.replace {
                    session.record_without_lock("new", args, &outcome);
                }
                return Err(outcome);
            }
        };
        let mut call = Call {
            command: "new",
            args,
            instance: None,
            started: now(),
            notes: Vec::new(),
            _lock: lock,
            cell: None,
        };
        let result = session
            .create_inner(&mut call, source, preset, create.replace, &workspace)
            .await;
        session.end(call, result)?;
        Ok(session)
    }

    async fn create_inner(
        &mut self,
        call: &mut Call,
        source: Source,
        preset: Preset,
        replace: bool,
        workspace: &Path,
    ) -> ToolResult<Outcome> {
        let paths = self.paths.clone();
        if replace {
            self.ensure_no_writer()?;
            let stubs = self
                .unfinished_instances(InstanceKind::Stub)
                .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the stub instances", e))?;
            if !stubs.is_empty() {
                return Err(Outcome::refused(
                    STAGE_INSTANCE,
                    problems::unfinished_stub(&stubs),
                ));
            }
            let mut marker: Marker = read_json(&paths.marker())
                .map_err(|e| Outcome::io(STAGE_PATHS, "read the sandbox marker", e))?;
            marker.ready = false;
            write_json(&paths.marker(), &marker)
                .map_err(|e| Outcome::io(STAGE_PATHS, "write the sandbox marker", e))?;
            let keep = [LOCK_NAME, MARKER_NAME, CALLS_DIR];
            let entries = std::fs::read_dir(paths.dir())
                .map_err(|e| Outcome::io(STAGE_PATHS, "list the sandbox directory", e))?;
            for entry in entries {
                let entry = entry.map_err(|e| Outcome::io(STAGE_PATHS, "list the sandbox", e))?;
                if keep.iter().any(|k| entry.file_name() == *k) {
                    continue;
                }
                let path = entry.path();
                let meta = std::fs::symlink_metadata(&path)
                    .map_err(|e| Outcome::io(STAGE_PATHS, "inspect the old tree", e))?;
                let removed = if meta.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                removed.map_err(|e| Outcome::io(STAGE_PATHS, "remove the old tree", e))?;
            }
            self.pause(PAUSE_REPLACE_AFTER_CLEAR).await;
        } else {
            let mut found = Vec::new();
            let entries = std::fs::read_dir(paths.dir())
                .map_err(|e| Outcome::io(STAGE_PATHS, "list the sandbox directory", e))?;
            for entry in entries {
                let entry = entry.map_err(|e| Outcome::io(STAGE_PATHS, "list the sandbox", e))?;
                if entry.file_name() != LOCK_NAME {
                    found.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
            if !found.is_empty() {
                found.sort();
                return Err(Outcome::refused(
                    STAGE_PATHS,
                    problems::not_empty(paths.dir(), &found),
                ));
            }
        }

        // The marker goes first, not ready, so a build that fails from
        // here on leaves a sandbox `new --replace` can rebuild.
        let mut marker = Marker {
            tool_version: TOOL_VERSION.to_string(),
            preset: preset.name().to_string(),
            source: source.kind().to_string(),
            created: now(),
            dir: paths.dir().display().to_string(),
            workspace: workspace.display().to_string(),
            ready: false,
        };
        write_json(&paths.marker(), &marker)
            .map_err(|e| Outcome::io(STAGE_PATHS, "write the sandbox marker", e))?;

        let host = Sandbox::create_at(&paths.host())
            .map_err(|e| Outcome::io(STAGE_PATHS, "build the host tree", e))?;
        preset::write_profile(&host)
            .map_err(|e| Outcome::io(STAGE_PATHS, "write the index profile", e))?;
        for dir in [
            paths.run(),
            paths.bin_dir(),
            paths.daemon_dir(),
            paths.stub_dir(),
            paths.preset_dir(),
            paths.cells(),
        ] {
            std::fs::create_dir_all(&dir)
                .map_err(|e| Outcome::io(STAGE_PATHS, "build the run tree", e))?;
        }
        let record = match &source {
            Source::Cargo => build::build_cargo(&paths, workspace).await?,
            Source::Prebuilt(bin) => build::install_copy(&paths, &bin.path, "prebuilt")?,
            Source::StandIn(path) => build::install_copy(&paths, path, "stand-in")?,
        };
        call.note(format!("binary: {} {}", record.kind, record.sha256));
        match preset {
            Preset::Bare => {
                preset::bare(&host)
                    .map_err(|e| Outcome::io(STAGE_PATHS, "lay out the preset", e))?;
                snapshot::switch(&paths, snapshot::Direction::Snapshot, &mut |_| Ok(()))?;
            }
            Preset::Rich => self.build_rich(call).await?,
        }
        marker.ready = true;
        write_json(&paths.marker(), &marker)
            .map_err(|e| Outcome::io(STAGE_PATHS, "write the sandbox marker", e))?;
        Ok(Outcome::ok())
    }

    /// Open the sandbox at `dir`, checking its marker. No lock is taken
    /// until a call.
    pub fn open(dir: &Path, opts: SessionOpts) -> ToolResult<Session> {
        let (paths, _) = Self::marker_prelock(dir)?;
        Ok(Session {
            paths,
            opts,
            stub_url: None,
            stubs: Vec::new(),
            daemon: None,
        })
    }

    pub fn paths(&self) -> &ToolPaths {
        &self.paths
    }

    /// The host tree, opened fresh.
    pub fn host(&self) -> ToolResult<Sandbox> {
        Sandbox::open(&self.paths.host())
            .map_err(|e| Outcome::refused(STAGE_PATHS, problems::path_refused(&e.to_string())))
    }

    /// The URL of the running stub, if any.
    pub fn stub_url(&self) -> Option<&str> {
        self.stub_url.as_deref()
    }

    /// The live daemon, if any.
    pub fn daemon(&self) -> Option<&LiveDaemon> {
        self.daemon.as_ref()
    }

    pub(crate) fn take_daemon(&mut self) -> Option<LiveDaemon> {
        self.daemon.take()
    }

    pub(crate) fn hold_daemon(&mut self, daemon: LiveDaemon) {
        self.daemon = Some(daemon);
    }

    /// Start an embed stub in this process and point every later
    /// command at it.
    pub fn stub_start(&mut self, spec: crate::embed_stub::StubSpec) -> String {
        let stub = EmbedStub::start_with(spec);
        let url = stub.url().to_string();
        self.stubs.push(stub);
        self.stub_url = Some(url.clone());
        url
    }

    /// The plan a command under test runs with, pointed at `stub_url`,
    /// the stub this call reaches (see `refresh_stub`).
    pub fn plan(&self, host: &Sandbox, dotenv: bool, stub_url: Option<&str>) -> EnvPlan {
        plan_for(
            &self.paths.bin(),
            host,
            stub_url,
            dotenv,
            std::env::vars_os(),
        )
    }

    /// Read and check the marker without the lock: the directory is the
    /// one the marker names, and the tool is the one that made it.
    fn marker_prelock(dir: &Path) -> ToolResult<(ToolPaths, Marker)> {
        let canonical = dir.canonicalize().map_err(|e| {
            Outcome::refused(STAGE_PATHS, problems::not_a_sandbox(dir, &e.to_string()))
        })?;
        let paths = ToolPaths::new(canonical);
        let marker: Marker = read_json(&paths.marker()).map_err(|e| {
            Outcome::refused(
                STAGE_PATHS,
                problems::not_a_sandbox(dir, &format!("no readable marker: {e}")),
            )
        })?;
        if Path::new(&marker.dir) != paths.dir() {
            return Err(Outcome::refused(
                STAGE_PATHS,
                problems::not_a_sandbox(
                    dir,
                    &format!(
                        "the marker names {}, not {}",
                        marker.dir,
                        paths.dir().display()
                    ),
                ),
            ));
        }
        if marker.tool_version != TOOL_VERSION {
            return Err(Outcome::refused(
                STAGE_PATHS,
                problems::not_a_sandbox(
                    dir,
                    &format!(
                        "the marker was written by tool version {}, this is {TOOL_VERSION}",
                        marker.tool_version
                    ),
                ),
            ));
        }
        Ok((paths, marker))
    }

    /// The marker again, under the lock: it must say the tree is whole.
    fn marker_ready(&self) -> ToolResult<Marker> {
        let (_, marker) = Self::marker_prelock(self.paths.dir())?;
        if !marker.ready {
            return Err(Outcome::refused(
                STAGE_PATHS,
                problems::not_ready(self.paths.dir()),
            ));
        }
        Ok(marker)
    }

    async fn resolve_workspace(given: Option<&Path>) -> ToolResult<PathBuf> {
        if let Some(path) = given {
            return path.canonicalize().map_err(|e| {
                Outcome::refused(STAGE_PATHS, problems::no_workspace(&e.to_string()))
            });
        }
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .await
            .map_err(|e| Outcome::refused(STAGE_PATHS, problems::no_workspace(&e.to_string())))?;
        if !output.status.success() {
            return Err(Outcome::refused(
                STAGE_PATHS,
                problems::no_workspace(&String::from_utf8_lossy(&output.stderr)),
            ));
        }
        let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
        PathBuf::from(top)
            .canonicalize()
            .map_err(|e| Outcome::refused(STAGE_PATHS, problems::no_workspace(&e.to_string())))
    }

    /// Take the session lock, retrying until the configured wait runs
    /// out.
    async fn acquire(&self, exclusive: bool) -> ToolResult<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.paths.lock())
            .map_err(|e| Outcome::io(STAGE_SESSION_LOCK, "open the session lock", e))?;
        let deadline = Instant::now() + self.opts.lock_wait;
        loop {
            let attempt = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            match attempt {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(Outcome::refused(
                            STAGE_SESSION_LOCK,
                            problems::session_lock_timeout(self.paths.dir(), self.opts.lock_wait),
                        ));
                    }
                    tokio::time::sleep(LOCK_POLL).await;
                }
                Err(TryLockError::Error(e)) => {
                    return Err(Outcome::io(STAGE_SESSION_LOCK, "lock the session lock", e));
                }
            }
        }
    }

    /// Start a call: take the lock, clean up what an interrupted switch
    /// left, check the marker says the tree is whole. A failure here is
    /// recorded and returned.
    pub(crate) async fn begin(
        &self,
        command: &'static str,
        args: Vec<String>,
        exclusive: bool,
    ) -> ToolResult<Call> {
        let lock = match self.acquire(exclusive).await {
            Ok(lock) => lock,
            Err(outcome) => {
                self.record_without_lock(command, args, &outcome);
                return Err(outcome);
            }
        };
        let mut call = Call {
            command,
            args,
            instance: None,
            started: now(),
            notes: Vec::new(),
            _lock: lock,
            cell: None,
        };
        if exclusive {
            match snapshot::cleanup_residue(&self.paths) {
                Ok(notes) => call.notes.extend(notes),
                Err(err) => {
                    let outcome = Outcome::io(STAGE_PATHS, "clean up an interrupted switch", err);
                    return Err(self.fail(call, outcome));
                }
            }
        }
        if let Err(outcome) = self.marker_ready() {
            return Err(self.fail(call, outcome));
        }
        Ok(call)
    }

    /// End a call: write its record and hand the outcome back, an
    /// `Err` staying an `Err`.
    pub(crate) fn end(&self, call: Call, result: ToolResult<Outcome>) -> ToolResult<Outcome> {
        match result {
            Ok(outcome) => {
                self.write_record(&call, &outcome)?;
                Ok(outcome)
            }
            Err(outcome) => Err(self.fail(call, outcome)),
        }
    }

    /// Record a failed call and return its outcome.
    fn fail(&self, call: Call, outcome: Outcome) -> Outcome {
        if let Err(write_err) = self.write_record(&call, &outcome) {
            return write_err;
        }
        outcome
    }

    /// A record for a call that never took the lock, written without it.
    fn record_without_lock(&self, command: &'static str, args: Vec<String>, outcome: &Outcome) {
        let record = Record {
            call: String::new(),
            command: command.to_string(),
            args,
            instance: None,
            class: outcome.class,
            stage: outcome.stage.to_string(),
            code: outcome.code,
            signal: outcome.signal,
            problem: outcome.problem.clone(),
            notes: Vec::new(),
            started: now(),
            finished: now(),
        };
        let _ = Self::persist_record(&self.paths, record, None);
    }

    fn write_record(&self, call: &Call, outcome: &Outcome) -> ToolResult<()> {
        let record = Record {
            call: String::new(),
            command: call.command.to_string(),
            args: call.args.clone(),
            instance: call.instance,
            class: outcome.class,
            stage: outcome.stage.to_string(),
            code: outcome.code,
            signal: outcome.signal,
            problem: outcome.problem.clone(),
            notes: call.notes.clone(),
            started: call.started,
            finished: now(),
        };
        Self::persist_record(&self.paths, record, call.cell.as_deref())
            .map_err(|e| Outcome::io(STAGE_TOOL, "write the call record", e))
    }

    /// Create `.calls/<id>/` and write the record there, and into the
    /// cell when there is one. The calls directory is checked level by
    /// level first, so a link planted there is refused rather than
    /// written through.
    fn persist_record(
        paths: &ToolPaths,
        mut record: Record,
        cell: Option<&Path>,
    ) -> io::Result<()> {
        let calls = paths.calls();
        paths
            .check(&calls, Expect::DirOrAbsent)
            .map_err(io::Error::other)?;
        if !calls.exists() {
            std::fs::create_dir(&calls)?;
        }
        let stamp = format!("{:.6}", now()).replace('.', "-");
        let mut id = format!("{stamp}-{}", std::process::id());
        let mut n = 0;
        let dir = loop {
            let candidate = calls.join(&id);
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    n += 1;
                    id = format!("{stamp}-{}-{n}", std::process::id());
                }
                Err(err) => return Err(err),
            }
        };
        record.call = id.clone();
        write_json(&dir.join("result.json"), &record)?;
        write_atomic(&calls.join("last"), format!("{id}\n").as_bytes())?;
        if let Some(cell) = cell {
            write_json(&cell.join("result.json"), &record)?;
        }
        Ok(())
    }

    /// The three questions a command that changes the tree asks first:
    /// the lock is held (by this call), no daemon start is unfinished,
    /// and no daemon holds the host's session lock.
    pub(crate) fn ensure_no_writer(&self) -> ToolResult<()> {
        let unfinished = self
            .unfinished_instances(InstanceKind::Daemon)
            .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the daemon instances", e))?;
        if !unfinished.is_empty() {
            return Err(Outcome::refused(
                STAGE_INSTANCE,
                problems::unfinished_instances(&unfinished),
            ));
        }
        let lock = self
            .paths
            .host()
            .join("runtime")
            .join(bookrack_session::tty_lock_name());
        match bookrack_session::lock_is_held(&lock) {
            Ok(false) => Ok(()),
            Ok(true) => Err(Outcome::refused(
                STAGE_INSTANCE,
                problems::session_lock_held(&lock),
            )),
            Err(err) => Err(Outcome::refused(
                STAGE_INSTANCE,
                problems::session_lock_unknown(&lock, &format!("{err:#}")),
            )),
        }
    }

    /// Instances of `kind` without an `exit.json` or `recovered.json`.
    pub(crate) fn unfinished_instances(&self, kind: InstanceKind) -> io::Result<Vec<u32>> {
        let mut out = Vec::new();
        for n in self.instances(kind)? {
            if !instance_ended(&self.paths.instance(kind, n)) {
                out.push(n);
            }
        }
        Ok(out)
    }

    /// Every instance number of `kind`, ascending. The directory holding
    /// them is checked level by level first, so a link planted in its
    /// place is refused rather than read through.
    pub(crate) fn instances(&self, kind: InstanceKind) -> io::Result<Vec<u32>> {
        let mut out = Vec::new();
        let dir = self.paths.instances_dir(kind);
        for level in [self.paths.run(), dir.clone()] {
            self.paths
                .check(&level, Expect::DirOrAbsent)
                .map_err(io::Error::other)?;
            if !level.exists() {
                return Ok(out);
            }
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(err) => return Err(err),
        };
        for entry in entries {
            let entry = entry?;
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            {
                out.push(n);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Hold at `point` if this session was told to: write the pause
    /// file and wait until something removes it.
    pub(crate) async fn pause(&self, point: &str) {
        if self.opts.pause_at.as_deref() != Some(point) {
            return;
        }
        let file = self.paths.pause(point);
        if std::fs::write(&file, b"").is_err() {
            return;
        }
        while file.exists() {
            tokio::time::sleep(PAUSE_POLL).await;
        }
    }

    /// Whether `id` may name a cell.
    pub fn valid_cell_id(id: &str) -> bool {
        let mut chars = id.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        id != "."
            && id != ".."
            && id.len() <= CELL_ID_MAX
            && first.is_ascii_alphanumeric()
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    }
}

/// Whether the start recorded at `instance` has ended: it has an
/// `exit.json` or a `recovered.json`.
pub(crate) fn instance_ended(instance: &Path) -> bool {
    instance.join("exit.json").exists() || instance.join("recovered.json").exists()
}

/// The plan the tool runs `bin` under against `host`.
///
/// The tool routes through the registry rather than a data root, points
/// the embedder at `stub_url` or the discard port, and switches dotenv
/// loading off unless `dotenv` says this run is the one that places a
/// `.env` in the sandbox's working directory. `parent` stands in for the
/// tool's own environment, the one the sweep works against.
pub fn plan_for(
    bin: &Path,
    host: &Sandbox,
    stub_url: Option<&str>,
    dotenv: bool,
    parent: impl Iterator<Item = (OsString, OsString)>,
) -> EnvPlan {
    let mut spawn = Spawn::for_tool(bin, host).ollama_url(stub_url.unwrap_or(NO_STUB_URL));
    if !dotenv {
        spawn = spawn.extra_env(NO_DOTENV_ENV, "1");
    }
    spawn.plan(parent)
}

/// A refusal from the tripwires.
pub(crate) fn tripped(findings: Vec<Finding>) -> Outcome {
    Outcome::refused(STAGE_TRIPWIRE, problems::tripped(&findings))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    /// Every kind maps to a class; the stage is always the failure's
    /// own, and an exit the harness saw is always carried.
    #[test]
    fn every_failure_kind_maps_to_a_class() {
        use crate::child::Output;
        let cases = [
            (
                FailureKind::Timeout,
                Stage::ReadyLine,
                Class::Timeout,
                "ready-line",
            ),
            (
                FailureKind::Timeout,
                Stage::ControlPlane,
                Class::Timeout,
                "control-plane",
            ),
            (FailureKind::NotReady, Stage::Run, Class::NotReady, "run"),
            (
                FailureKind::NotReady,
                Stage::DaemonLock,
                Class::NotReady,
                "daemon-lock",
            ),
            (
                FailureKind::NotReady,
                Stage::Endpoint,
                Class::NotReady,
                "endpoint",
            ),
            (
                FailureKind::CaptureFailed,
                Stage::Capture,
                Class::CaptureFailed,
                "capture",
            ),
            (
                FailureKind::CaptureFailed,
                Stage::Drain,
                Class::CaptureFailed,
                "drain",
            ),
            (FailureKind::WaitFailed, Stage::Run, Class::ToolError, "run"),
            (
                FailureKind::WaitFailed,
                Stage::Stop,
                Class::ToolError,
                "stop",
            ),
            (
                FailureKind::ReclaimFailed,
                Stage::Run,
                Class::ToolError,
                "run",
            ),
        ];
        for (kind, stage, class, name) in cases {
            for exit in [
                None,
                Some(ExitStatus::from_raw(3 << 8)),
                Some(ExitStatus::from_raw(15)),
            ] {
                let failure = Failure {
                    kind,
                    stage,
                    exit,
                    why: "why".to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    output: Output::Memory {
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    },
                };
                let outcome = Outcome::from_failure(&failure);
                assert_eq!(outcome.class, class, "{kind:?} at {stage:?}");
                assert_eq!(outcome.stage, name, "{kind:?} at {stage:?}");
                match exit {
                    None => assert_eq!((outcome.code, outcome.signal), (None, None)),
                    Some(status) if status.code().is_some() => {
                        assert_eq!(outcome.code, Some(3), "{kind:?} with an exit code");
                    }
                    Some(_) => assert_eq!(outcome.signal, Some(15), "{kind:?} with a signal"),
                }
                assert_eq!(
                    outcome
                        .problem
                        .as_ref()
                        .and_then(|p| p.data.detail.as_deref()),
                    Some("why")
                );
            }
        }
        let exited = Outcome::from_exit(ExitStatus::from_raw(124 << 8));
        assert_eq!(
            (exited.class, exited.code, exited.exit_code()),
            (Class::Exited, Some(124), 124)
        );
        let signaled = Outcome::from_exit(ExitStatus::from_raw(15));
        assert_eq!(
            (signaled.class, signaled.signal, signaled.exit_code()),
            (Class::Signaled, Some(15), 143)
        );
    }

    #[test]
    fn a_cell_id_is_a_short_plain_name() {
        for ok in ["c1", "w1", "a.b-c_d", "0"] {
            assert!(Session::valid_cell_id(ok), "{ok:?}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "/tmp/x",
            "-x",
            ".hidden",
            &"a".repeat(65),
        ] {
            assert!(!Session::valid_cell_id(bad), "{bad:?}");
        }
    }

    /// Tool-owned paths are checked level by level: a link at any level
    /// is refused, and a file with a second link is refused.
    #[test]
    fn tool_paths_refuse_links_at_any_level() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ToolPaths::new(dir.path().canonicalize().expect("canonical"));
        std::fs::create_dir(paths.cells()).expect("cells");
        paths
            .check(&paths.cells(), Expect::Dir)
            .expect("a real directory");
        paths
            .check(&paths.cell("c"), Expect::Absent)
            .expect("absent");
        paths
            .check(&paths.summary(), Expect::FileOrAbsent)
            .expect("absent file");

        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(outside.path().join("shared"), b"x").expect("write");
        std::fs::hard_link(outside.path().join("shared"), paths.summary()).expect("hard link");
        let err = paths
            .check(&paths.summary(), Expect::File)
            .expect_err("a second link is refused");
        assert!(err.contains("2 links"), "{err}");

        std::fs::remove_dir_all(paths.cells()).expect("remove");
        std::os::unix::fs::symlink(outside.path(), paths.cells()).expect("symlink");
        let err = paths
            .check(&paths.cell("c"), Expect::Absent)
            .expect_err("a linked directory on the way is refused");
        assert!(err.contains("symbolic link"), "{err}");
    }
}
