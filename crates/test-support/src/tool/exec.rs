// SPDX-License-Identifier: Apache-2.0

//! The commands a [`Session`] runs inside its sandbox: a client command
//! under the tripwires with its artifacts kept, a daemon brought up and
//! stopped, the tripwires alone, a rebuild, a snapshot or restore.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use bookrack_core::Problem;
use serde::Serialize;
use tokio::process::Command;

use super::build::{self, BinaryRecord};
use super::snapshot::{self, Direction};
use super::tripwire::{self, BinaryView, Finding, Input, PlacedDotenv};
use super::{
    Call, Class, Expect, LiveDaemon, Outcome, STAGE_INSTANCE, STAGE_PATHS, STAGE_SPAWN, Session,
    ToolResult, env, now, problems, sig, stage_name, tripped, write_atomic, write_json,
};
use crate::child::{CaptureMode, DaemonProcess, Output, Ready, Sink, SpawnOpts, Stage};
use crate::sandbox::Sandbox;
use crate::spawn::EnvPlan;

/// How long a client command may run unless told otherwise.
// setting: internal -- the tool's default exec deadline, overridden by --timeout
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a daemon may take to become ready unless told otherwise.
// setting: internal -- the tool's default daemon readiness deadline, overridden per start
pub const DEFAULT_DAEMON_TIMEOUT: Duration = Duration::from_secs(30);

/// The most of a command's first stdout line the summary row keeps.
// setting: internal -- bounds the summary row's stdout excerpt, not a value to tune
const SUMMARY_EXCERPT: usize = 80;

/// Where a command's output goes when it has no cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Kept in memory and handed back.
    Memory,
    /// Streamed to this process's own stdout and stderr as it is read.
    Inherit,
}

/// How one client command is run.
#[derive(Debug, Clone)]
pub struct ExecOpts {
    /// Keep the command's artifacts under `cells/<id>/`.
    pub cell: Option<String>,
    pub timeout: Duration,
    /// A file under the sandbox to feed the command as stdin.
    pub stdin: Option<PathBuf>,
    /// A file under the sandbox to place as the child's `.env`.
    pub dotenv: Option<PathBuf>,
    /// Where output goes without a cell.
    pub capture: Capture,
}

impl Default for ExecOpts {
    fn default() -> Self {
        ExecOpts {
            cell: None,
            timeout: DEFAULT_EXEC_TIMEOUT,
            stdin: None,
            dotenv: None,
            capture: Capture::Memory,
        }
    }
}

impl ExecOpts {
    pub fn cell(id: &str) -> ExecOpts {
        ExecOpts {
            cell: Some(id.to_string()),
            ..ExecOpts::default()
        }
    }
}

/// What one client command left.
#[derive(Debug)]
pub struct ExecOutcome {
    pub outcome: Outcome,
    /// What the harness kept of the command's streams.
    pub output: Output,
    /// The cell directory, when there was one.
    pub cell: Option<PathBuf>,
}

/// How a daemon is started.
#[derive(Debug, Clone)]
pub struct DaemonStart {
    library: Option<String>,
    dotenv: Option<PathBuf>,
    timeout: Duration,
}

impl Default for DaemonStart {
    fn default() -> Self {
        DaemonStart {
            library: None,
            dotenv: None,
            timeout: DEFAULT_DAEMON_TIMEOUT,
        }
    }
}

impl DaemonStart {
    /// Serve `name`, passed as `--library`.
    pub fn library(name: &str) -> DaemonStart {
        DaemonStart {
            library: Some(name.to_string()),
            ..DaemonStart::default()
        }
    }

    /// Place `path` as the daemon's `.env` for its start.
    pub fn dotenv(mut self, path: &Path) -> DaemonStart {
        self.dotenv = Some(path.to_path_buf());
        self
    }

    pub fn timeout(mut self, within: Duration) -> DaemonStart {
        self.timeout = within;
        self
    }

    fn args(&self) -> Vec<OsString> {
        let mut args = vec![OsString::from("run")];
        if let Some(library) = &self.library {
            args.push("--library".into());
            args.push(library.into());
        }
        args
    }
}

/// What a daemon instance records when it starts.
#[derive(Serialize)]
struct StartRecord {
    args: Vec<String>,
    dotenv: Option<String>,
    started: f64,
    parent_pid: u32,
}

/// What a daemon instance records once it is ready.
#[derive(Serialize)]
struct ReadyRecord {
    pid: u32,
    control_sock: String,
    ready_line: String,
}

/// What a daemon instance records when its start has ended.
#[derive(Serialize)]
struct ExitRecord {
    class: Class,
    stage: String,
    code: Option<i32>,
    signal: Option<i32>,
    detail: Option<String>,
    finished: f64,
}

impl ExitRecord {
    fn from(outcome: &Outcome) -> ExitRecord {
        ExitRecord {
            class: outcome.class,
            stage: outcome.stage.to_string(),
            code: outcome.code,
            signal: outcome.signal,
            detail: outcome.problem.as_ref().and_then(|p| p.data.detail.clone()),
            finished: now(),
        }
    }
}

/// Removes the `.env` a run placed, on every path out.
struct DotenvGuard(PathBuf);

impl Drop for DotenvGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// What the four preflight steps leave for the command to run with.
struct Preflight {
    host: Sandbox,
    plan: EnvPlan,
    _dotenv: Option<DotenvGuard>,
}

impl Session {
    /// The four steps every command under test goes through, in this
    /// order: the tool's own paths, the pure tripwires without T4, the
    /// `.env` check and placement with T4, then T7.
    async fn preflight(
        &self,
        args: &[OsString],
        cell: Option<&str>,
        stdin: Option<&Path>,
        dotenv: Option<&Path>,
    ) -> ToolResult<Preflight> {
        let paths = self.paths();
        let refuse = |why: String| Outcome::refused(STAGE_PATHS, problems::path_refused(&why));

        // 1. Paths the tool owns, before anything is written.
        paths.check(&paths.run(), Expect::Dir).map_err(refuse)?;
        paths.check(&paths.cells(), Expect::Dir).map_err(refuse)?;
        paths.check(&paths.host(), Expect::Dir).map_err(refuse)?;
        paths
            .check(&paths.host().join("cwd"), Expect::Dir)
            .map_err(refuse)?;
        paths
            .check(&paths.summary(), Expect::FileOrAbsent)
            .map_err(refuse)?;
        for source in [stdin, dotenv].into_iter().flatten() {
            locate_input(paths, source).map_err(refuse)?;
        }
        if let Some(id) = cell {
            paths
                .check(&paths.cell(id), Expect::Absent)
                .map_err(refuse)?;
            std::fs::create_dir(paths.cell(id))
                .map_err(|e| Outcome::io(STAGE_PATHS, "create the cell", e))?;
        }
        let placed_content = match dotenv {
            Some(source) => Some(
                std::fs::read(source)
                    .map_err(|e| Outcome::io(STAGE_PATHS, "read the .env to place", e))?,
            ),
            None => None,
        };

        // 2. The pure tripwires, T4 excepted: T3 walks the tree before
        //    anything is written into host/.
        let host = self.host()?;
        let plan = self.plan(&host, dotenv.is_some());
        let env = plan.composed(std::env::vars_os());
        let placed = placed_content.map(|content| PlacedDotenv {
            path: host.cwd().join(".env"),
            content,
        });
        let input = Input {
            dir: paths.dir(),
            host: &host,
            env: &env,
            argv: args,
            stub_url: self.stub_url(),
            dotenv: placed.as_ref(),
        };
        let findings = tripwire::check_before_dotenv(&input);
        if !findings.is_empty() {
            return Err(tripped(findings));
        }

        // 3. The .env: none may be there already; place this run's, then
        //    judge the chain with T4.
        let env_path = host.cwd().join(".env");
        if std::fs::symlink_metadata(&env_path).is_ok() {
            return Err(Outcome::refused(
                STAGE_PATHS,
                problems::leftover_dotenv(&env_path),
            ));
        }
        let guard = match &placed {
            Some(placed) => {
                write_atomic(&placed.path, &placed.content)
                    .map_err(|e| Outcome::io(STAGE_PATHS, "place the .env", e))?;
                Some(DotenvGuard(placed.path.clone()))
            }
            None => None,
        };
        let findings = tripwire::check_dotenv(&host.cwd(), placed.as_ref());
        if !findings.is_empty() {
            return Err(tripped(findings));
        }

        // 4. The binary's own view.
        let stripped = super::argv::strip_globals(args).map_err(|f| tripped(vec![f]))?;
        let mut cmd = std::process::Command::new(paths.bin());
        plan.apply(&mut cmd);
        cmd.args(tripwire::t7_command(&stripped));
        let view = BinaryView {
            host_root: host.path(),
            cwd: &host.cwd(),
            dotenv: placed.as_ref().map(|p| p.path.as_path()),
        };
        let findings = tripwire::run_binary_view(Command::from(cmd), &view).await;
        if !findings.is_empty() {
            return Err(tripped(findings));
        }
        Ok(Preflight {
            host,
            plan,
            _dotenv: guard,
        })
    }

    /// Run one client command under the tripwires.
    pub async fn exec(&mut self, args: &[OsString], opts: ExecOpts) -> ToolResult<ExecOutcome> {
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut call = self.begin("exec", shown, true).await?;
        let mut output = None;
        let result = self.exec_inner(&mut call, args, &opts, &mut output).await;
        let cell = call.cell.clone();
        let outcome = self.end(call, result)?;
        Ok(ExecOutcome {
            outcome,
            output: output.unwrap_or(Output::Memory {
                stdout: Vec::new(),
                stderr: Vec::new(),
            }),
            cell,
        })
    }

    async fn exec_inner(
        &self,
        call: &mut Call,
        args: &[OsString],
        opts: &ExecOpts,
        output: &mut Option<Output>,
    ) -> ToolResult<Outcome> {
        let paths = self.paths().clone();
        if let Some(id) = &opts.cell
            && !Session::valid_cell_id(id)
        {
            return Err(Outcome::refused(STAGE_PATHS, problems::bad_cell_id(id)));
        }
        build::verify(&paths)?;
        let pre = self
            .preflight(
                args,
                opts.cell.as_deref(),
                opts.stdin.as_deref(),
                opts.dotenv.as_deref(),
            )
            .await?;
        let cell_dir = opts.cell.as_deref().map(|id| paths.cell(id));
        if let Some(cell) = &cell_dir {
            call.set_cell(cell.clone());
        }

        let io_err = |what: &str, e: std::io::Error| Outcome::io(STAGE_PATHS, what, e);
        let (sig_before, registry_before, platform_before) = match &cell_dir {
            Some(cell) => {
                let shown: Vec<String> = args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                write_atomic(
                    &cell.join("cmd"),
                    format!("{}\n", shown.join(" ")).as_bytes(),
                )
                .map_err(|e| io_err("write the cell's cmd", e))?;
                let sig_before =
                    sig::signature(&paths.host()).map_err(|e| io_err("sign the host tree", e))?;
                write_atomic(&cell.join("sig.before"), sig_before.join("\n").as_bytes())
                    .map_err(|e| io_err("write sig.before", e))?;
                let registry_before = std::fs::read(pre.host.registry_path()).unwrap_or_default();
                write_atomic(&cell.join("registry.before"), &registry_before)
                    .map_err(|e| io_err("write registry.before", e))?;
                let platform_before = platform_registry_digest();
                write_atomic(
                    &cell.join("platform-registry.before"),
                    platform_before.as_bytes(),
                )
                .map_err(|e| io_err("write platform-registry.before", e))?;
                (
                    Some(sig_before),
                    Some(registry_before),
                    Some(platform_before),
                )
            }
            None => (None, None, None),
        };

        let mut cmd = std::process::Command::new(paths.bin());
        pre.plan.apply(&mut cmd);
        cmd.args(args);
        let stdin = match &opts.stdin {
            Some(path) => Stdio::from(
                std::fs::File::open(path).map_err(|e| io_err("open the stdin file", e))?,
            ),
            None => Stdio::null(),
        };
        let capture = match (&cell_dir, opts.capture) {
            (Some(cell), _) => CaptureMode::Stream {
                stdout: Sink::File(
                    std::fs::File::create(cell.join("stdout"))
                        .map_err(|e| io_err("create stdout", e))?,
                ),
                stderr: Sink::File(
                    std::fs::File::create(cell.join("stderr"))
                        .map_err(|e| io_err("create stderr", e))?,
                ),
            },
            (None, Capture::Inherit) => CaptureMode::Stream {
                stdout: Sink::Stdout,
                stderr: Sink::Stderr,
            },
            (None, Capture::Memory) => CaptureMode::Memory,
        };
        let started = Instant::now();
        let child = DaemonProcess::spawn_with(Command::from(cmd), SpawnOpts { stdin, capture })
            .map_err(|e| Outcome::io(STAGE_SPAWN, "start the command under test", e))?;
        let (outcome, out, excerpt) = match child.exit_within(opts.timeout).await {
            Ok(exit) => {
                let excerpt = exit.stdout.lines().next().unwrap_or("").to_string();
                (Outcome::from_exit(exit.status), exit.output, excerpt)
            }
            Err(failure) => {
                let excerpt = failure.stdout.lines().next().unwrap_or("").to_string();
                (Outcome::from_failure(&failure), failure.output, excerpt)
            }
        };
        let elapsed = started.elapsed();
        *output = Some(out);

        if let (Some(cell), Some(sig_before), Some(registry_before), Some(platform_before)) =
            (&cell_dir, sig_before, registry_before, platform_before)
        {
            let sig_after =
                sig::signature(&paths.host()).map_err(|e| io_err("sign the host tree", e))?;
            write_atomic(&cell.join("sig.after"), sig_after.join("\n").as_bytes())
                .map_err(|e| io_err("write sig.after", e))?;
            let registry_after = std::fs::read(pre.host.registry_path()).unwrap_or_default();
            write_atomic(&cell.join("registry.after"), &registry_after)
                .map_err(|e| io_err("write registry.after", e))?;
            let platform_after = platform_registry_digest();
            write_atomic(
                &cell.join("platform-registry.after"),
                platform_after.as_bytes(),
            )
            .map_err(|e| io_err("write platform-registry.after", e))?;
            let tree_change = sig::compact_diff(&sig::diff(&sig_before, &sig_after));
            let row = [
                opts.cell.clone().unwrap_or_default(),
                sig::escape(
                    args.iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(" ")
                        .as_bytes(),
                ),
                serde_json::to_value(outcome.class)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                outcome
                    .code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                sig::escape(
                    excerpt
                        .chars()
                        .take(SUMMARY_EXCERPT)
                        .collect::<String>()
                        .as_bytes(),
                ),
                tree_change,
                if registry_before == registry_after {
                    "same"
                } else {
                    "changed"
                }
                .to_string(),
                if platform_before == platform_after {
                    "same"
                } else {
                    "changed"
                }
                .to_string(),
                format!("{:.3}", elapsed.as_secs_f64()),
            ];
            sig::append_line(&paths, &paths.summary(), &row.join("\t"))
                .map_err(|e| io_err("append to summary.tsv", e))?;
        }
        Ok(outcome)
    }

    /// Run the tripwires for `args` without running the command.
    pub async fn tripwire(&self, args: &[OsString]) -> ToolResult<Vec<Finding>> {
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let call = self.begin("tripwire", shown, false).await?;
        let result = async {
            let host = self.host()?;
            let plan = self.plan(&host, false);
            let env = plan.composed(std::env::vars_os());
            let input = Input {
                dir: self.paths().dir(),
                host: &host,
                env: &env,
                argv: args,
                stub_url: self.stub_url(),
                dotenv: None,
            };
            let mut findings = tripwire::check(&input);
            if findings.is_empty() {
                build::verify(self.paths())?;
                let stripped = super::argv::strip_globals(args).map_err(|f| tripped(vec![f]))?;
                let mut cmd = std::process::Command::new(self.paths().bin());
                plan.apply(&mut cmd);
                cmd.args(tripwire::t7_command(&stripped));
                let view = BinaryView {
                    host_root: host.path(),
                    cwd: &host.cwd(),
                    dotenv: None,
                };
                findings = tripwire::run_binary_view(Command::from(cmd), &view).await;
            }
            Ok(findings)
        }
        .await;
        match result {
            Ok(findings) if findings.is_empty() => {
                self.end(call, Ok(Outcome::ok()))?;
                Ok(findings)
            }
            Ok(findings) => {
                self.end(call, Ok(tripped(findings.clone())))?;
                Ok(findings)
            }
            Err(outcome) => Err(self.end(call, Err(outcome)).unwrap_err()),
        }
    }

    /// The plan as shell lines, for debugging by hand.
    pub async fn env_lines(&self, dotenv: bool) -> ToolResult<String> {
        let call = self.begin("env", vec![dotenv.to_string()], false).await?;
        let result = self
            .host()
            .map(|host| env::shell_lines(&self.plan(&host, dotenv), dotenv));
        match result {
            Ok(lines) => {
                self.end(call, Ok(Outcome::ok()))?;
                Ok(lines)
            }
            Err(outcome) => Err(self.end(call, Err(outcome)).unwrap_err()),
        }
    }

    /// Rebuild or re-copy the binary under test.
    pub async fn build(&mut self) -> ToolResult<BinaryRecord> {
        let call = self.begin("build", Vec::new(), true).await?;
        let result = async {
            self.ensure_no_writer()?;
            let marker = super::Session::marker_prelock(self.paths().dir())?.1;
            let paths = self.paths().clone();
            match marker.source.as_str() {
                "cargo" => build::build_cargo(&paths, Path::new(&marker.workspace)).await,
                kind => {
                    let record = build::read_record(&paths)?;
                    build::install_copy(&paths, Path::new(&record.origin), kind)
                }
            }
        }
        .await;
        match result {
            Ok(record) => {
                self.end(call, Ok(Outcome::ok()))?;
                Ok(record)
            }
            Err(outcome) => Err(self.end(call, Err(outcome)).unwrap_err()),
        }
    }

    /// Copy `host/` to `golden/`.
    pub async fn snapshot(&mut self) -> ToolResult<Outcome> {
        self.switch("snapshot", Direction::Snapshot).await
    }

    /// Copy `golden/` back over `host/`.
    pub async fn restore(&mut self) -> ToolResult<Outcome> {
        self.switch("restore", Direction::Restore).await
    }

    async fn switch(&mut self, command: &'static str, direction: Direction) -> ToolResult<Outcome> {
        let call = self.begin(command, Vec::new(), true).await?;
        let result = self
            .ensure_no_writer()
            .and_then(|()| snapshot::switch(self.paths(), direction, &mut |_| Ok(())))
            .map(|()| Outcome::ok());
        self.end(call, result)
    }

    /// Start a daemon and hold it. Nothing is recorded as an instance
    /// until every check has passed; from then on, every way the start
    /// can fail leaves an `exit.json`.
    pub async fn daemon_start(&mut self, start: DaemonStart) -> ToolResult<Ready> {
        let args = start.args();
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut call = self.begin("daemon start", shown.clone(), true).await?;
        let mut ready = None;
        let result = self
            .daemon_start_inner(&mut call, &start, &args, &shown, &mut ready)
            .await;
        let outcome = self.end(call, result)?;
        match ready {
            Some(ready) => Ok(ready),
            None => Err(outcome),
        }
    }

    async fn daemon_start_inner(
        &mut self,
        call: &mut Call,
        start: &DaemonStart,
        args: &[OsString],
        shown: &[String],
        ready_out: &mut Option<Ready>,
    ) -> ToolResult<Outcome> {
        let paths = self.paths().clone();
        self.ensure_no_writer()?;
        if self.daemon().is_some() {
            return Err(Outcome::refused(
                STAGE_INSTANCE,
                Problem::new("a daemon this session started is still held")
                    .hint("stop it with daemon_stop first"),
            ));
        }
        build::verify(&paths)?;
        let pre = self
            .preflight(args, None, None, start.dotenv.as_deref())
            .await?;

        let n = self
            .instances()
            .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the daemon instances", e))?
            .last()
            .map_or(1, |n| n + 1);
        let instance = paths.daemon_instance(n);
        std::fs::create_dir_all(&instance)
            .map_err(|e| Outcome::io(STAGE_INSTANCE, "create the daemon instance", e))?;
        call.set_instance(n);
        write_json(
            &instance.join("start.json"),
            &StartRecord {
                args: shown.to_vec(),
                dotenv: start.dotenv.as_ref().map(|p| p.display().to_string()),
                started: now(),
                parent_pid: std::process::id(),
            },
        )
        .map_err(|e| Outcome::io(STAGE_INSTANCE, "write start.json", e))?;

        // From here on every failure writes exit.json.
        let exit_record = |outcome: &Outcome| {
            let _ = write_json(&instance.join("exit.json"), &ExitRecord::from(outcome));
        };
        let mut cmd = std::process::Command::new(paths.bin());
        pre.plan.apply(&mut cmd);
        cmd.args(args);
        let sinks = (|| -> std::io::Result<(Sink, Sink)> {
            Ok((
                Sink::File(std::fs::File::create(instance.join("stdout"))?),
                Sink::File(std::fs::File::create(instance.join("stderr"))?),
            ))
        })();
        let (stdout, stderr) = match sinks {
            Ok(sinks) => sinks,
            Err(e) => {
                let outcome = Outcome::io(STAGE_SPAWN, "create the daemon's output files", e);
                exit_record(&outcome);
                return Err(outcome);
            }
        };
        let mut daemon = match DaemonProcess::spawn_with(
            Command::from(cmd),
            SpawnOpts {
                stdin: Stdio::null(),
                capture: CaptureMode::Stream { stdout, stderr },
            },
        ) {
            Ok(daemon) => daemon,
            Err(e) => {
                let outcome = Outcome::io(STAGE_SPAWN, "start the daemon under test", e);
                exit_record(&outcome);
                return Err(outcome);
            }
        };
        let lock = pre.host.tty_lock_path();
        match daemon.probe_ready(&lock, start.timeout).await {
            Ok(ready) => {
                write_json(
                    &instance.join("daemon.json"),
                    &ReadyRecord {
                        pid: ready.pid,
                        control_sock: ready.control_sock.display().to_string(),
                        ready_line: ready.ready_line.clone(),
                    },
                )
                .map_err(|e| Outcome::io(STAGE_INSTANCE, "write daemon.json", e))?;
                self.hold_daemon(LiveDaemon {
                    instance: n,
                    process: daemon,
                    control_sock: ready.control_sock.clone(),
                });
                *ready_out = Some(ready);
                Ok(Outcome::ok())
            }
            Err(not_ready) => {
                let failure = daemon.give_up(not_ready).await;
                let outcome = Outcome::from_failure(&failure);
                exit_record(&outcome);
                Err(outcome)
            }
        }
    }

    /// Stop the held daemon with `SIGTERM`, recording how it ended.
    pub async fn daemon_stop(&mut self, within: Duration) -> ToolResult<Outcome> {
        let mut call = self
            .begin("daemon stop", vec![format!("{within:?}")], true)
            .await?;
        let Some(live) = self.take_daemon() else {
            let outcome = Outcome::refused(
                STAGE_INSTANCE,
                Problem::new("no daemon this session started is running")
                    .hint("start one with daemon_start"),
            );
            return self.end(call, Err(outcome));
        };
        call.set_instance(live.instance);
        let instance = self.paths().daemon_instance(live.instance);
        let outcome = match live.process.stop(within).await {
            Ok(exit) => Outcome::from_exit(exit.status),
            Err(failure) => {
                let mut outcome = Outcome::from_failure(&failure);
                if outcome.stage == stage_name(Stage::Run) {
                    outcome.stage = stage_name(Stage::Stop);
                }
                outcome
            }
        };
        if let Err(e) = write_json(&instance.join("exit.json"), &ExitRecord::from(&outcome)) {
            return self.end(call, Err(Outcome::io(STAGE_INSTANCE, "write exit.json", e)));
        }
        self.end(call, Ok(outcome))
    }
}

/// The digest of the real platform registry, in the tool's own
/// environment, or `absent`.
fn platform_registry_digest() -> String {
    match bookrack_config::default_registry_path() {
        Some(path) => sig::sha256_file(&path).unwrap_or_else(|_| "absent".to_string()),
        None => "absent".to_string(),
    }
}

/// Where an input file given to the tool really is: its directory
/// resolved, its own name left alone so a link in place of the file is
/// still seen. It must lie under the sandbox and be a plain file the
/// tool's path check accepts.
fn locate_input(paths: &super::ToolPaths, source: &Path) -> Result<PathBuf, String> {
    let absolute = if source.is_absolute() {
        source.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(source)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", absolute.display()))?;
    let name = absolute
        .file_name()
        .ok_or_else(|| format!("{} has no file name", absolute.display()))?;
    let parent = parent
        .canonicalize()
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    let located = parent.join(name);
    if !located.starts_with(paths.dir()) {
        return Err(format!(
            "{} is not under the sandbox; put input files in host/inputs/",
            source.display()
        ));
    }
    paths.check(&located, Expect::File)?;
    Ok(located)
}
