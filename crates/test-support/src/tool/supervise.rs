// SPDX-License-Identifier: Apache-2.0

//! A daemon held by a supervisor in the background, so a sandbox's
//! daemon outlives the `daemon start` command that brought it up.
//!
//! The supervisor is the daemon's parent and its only observer: it
//! spawns the daemon from the plan the start judged, probes it ready,
//! watches it, and stops it on request, or unasked once the sandbox is
//! idle or gone. Every way a start ends, the supervisor writes
//! `exit.json` as its last write. It answers requests
//! on `supervisor.sock` in its instance directory, one request and one
//! answer per connection, each a line of JSON.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use bookrack_core::Problem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::detach::{
    self, HANDOFF_DEADLINE, Reason, STATE_POLL, WATCH_POLL, go_refusal, handoff_exit, read_state,
    read_state_opt, register, watch, write_recovered,
};
use super::exec::{
    DaemonStart, ExitRecord, Hosting, ReadyRecord, SpawnRecord, StartRecord, finish_spawn,
    record_spawn, stop_outcome,
};
use super::recover::DEFAULT_STOP_TIMEOUT;
use super::{
    Call, InstanceKind, Outcome, PAUSE_START_AFTER_INTENT, PAUSE_SUPERVISE_AFTER_SPAWN,
    STAGE_INSTANCE, STAGE_SPAWN, STAGE_SUPERVISOR, Session, SessionOpts, ToolResult, build, now,
    sig, write_json,
};
use crate::child::{
    CaptureMode, DaemonProcess, FailureKind, ReadyFailure, Sink, SpawnOpts, Stage, TEARDOWN_BUDGET,
};

/// How long a client waits for a supervisor's answer to `status`.
// setting: internal -- the supervisor status request deadline, not a value to tune
pub(crate) const STATUS_DEADLINE: Duration = Duration::from_secs(2);

/// What a client adds on top of the most a supervisor may take, so a
/// supervisor at its own bound still answers in time.
// setting: internal -- the client's margin over a supervisor's own bound, not a value to tune
pub(crate) const CLIENT_MARGIN: Duration = Duration::from_secs(2);

/// The longest request line a supervisor reads.
// setting: internal -- bounds a supervisor request, not a value to tune
const REQUEST_CAP: u64 = 4096;

/// Where a supervisor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Registered, waiting for its `go`.
    Handoff,
    /// The daemon is spawned and being probed.
    Starting,
    /// The daemon is ready.
    Running,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Handoff => "handoff",
            Phase::Starting => "starting",
            Phase::Running => "running",
        }
    }
}

/// One request a supervisor reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Status,
    Stop {
        within: Duration,
        /// Whether to send `SIGTERM`; `false` waits for an exit the
        /// daemon was asked for another way.
        signal: bool,
    },
    /// Anything else, with what was wrong with it.
    Unknown(String),
}

/// What a supervisor does with a request in a phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Answer with where it is.
    Status,
    /// Leave without starting: write `exit.json`, answer, exit.
    AbortHandoff,
    /// Drop the probe, reclaim the daemon, write `exit.json`, answer,
    /// exit.
    CancelStart,
    /// Stop the ready daemon within the duration, write `exit.json`,
    /// answer, exit.
    Stop(Duration),
    /// Wait for the ready daemon to exit within the duration without
    /// signalling it, write `exit.json`, answer, exit.
    AwaitExit(Duration),
    /// Answer with an error and go on.
    Refuse(String),
}

/// The supervisor's answer to `request` in `phase`.
pub fn respond(phase: Phase, request: &Request) -> Action {
    match (phase, request) {
        (_, Request::Status) => Action::Status,
        (Phase::Handoff, Request::Stop { .. }) => Action::AbortHandoff,
        (Phase::Starting, Request::Stop { .. }) => Action::CancelStart,
        (
            Phase::Running,
            Request::Stop {
                within,
                signal: true,
            },
        ) => Action::Stop(*within),
        (
            Phase::Running,
            Request::Stop {
                within,
                signal: false,
            },
        ) => Action::AwaitExit(*within),
        (_, Request::Unknown(why)) => Action::Refuse(why.clone()),
    }
}

/// Read one request line.
pub fn parse_request(line: &str) -> Request {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => return Request::Unknown(format!("the request is not JSON: {e}")),
    };
    match value.get("op").and_then(Value::as_str) {
        Some("status") => Request::Status,
        Some("stop") => match value.get("within_ms").and_then(Value::as_u64) {
            Some(ms) => Request::Stop {
                within: Duration::from_millis(ms),
                signal: value.get("signal").and_then(Value::as_bool).unwrap_or(true),
            },
            None => Request::Unknown("a stop needs within_ms".to_string()),
        },
        Some(op) => Request::Unknown(format!("unknown op {op:?}")),
        None => Request::Unknown("the request names no op".to_string()),
    }
}

/// The plan a supervisor starts the daemon with, `plan.json`: the
/// environment the start's tripwires judged, its working directory, and
/// its arguments, every byte kept through [`sig::escape`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFile {
    pub env: Vec<[String; 2]>,
    pub cwd: String,
    pub args: Vec<String>,
}

/// What a [`PlanFile`] holds, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub args: Vec<OsString>,
}

impl PlanFile {
    pub fn encode(plan: &Plan) -> PlanFile {
        let esc = |s: &std::ffi::OsStr| sig::escape(s.as_bytes());
        PlanFile {
            env: plan.env.iter().map(|(k, v)| [esc(k), esc(v)]).collect(),
            cwd: esc(plan.cwd.as_os_str()),
            args: plan.args.iter().map(|a| esc(a)).collect(),
        }
    }

    pub fn decode(&self) -> Result<Plan, String> {
        let os = |s: &str| sig::unescape(s).map(OsString::from_vec);
        Ok(Plan {
            env: self
                .env
                .iter()
                .map(|[k, v]| Ok((os(k)?, os(v)?)))
                .collect::<Result<_, String>>()?,
            cwd: PathBuf::from(os(&self.cwd)?),
            args: self
                .args
                .iter()
                .map(|a| os(a))
                .collect::<Result<_, String>>()?,
        })
    }
}

/// Send `request` to the supervisor at `sock` and read its answer, all
/// within `within`.
pub(crate) async fn ask(sock: &Path, request: &Value, within: Duration) -> Result<Value, String> {
    let exchange = async {
        let mut stream = UnixStream::connect(sock)
            .await
            .map_err(|e| format!("connecting to {} failed: {e}", sock.display()))?;
        let mut line = request.to_string();
        line.push('\n');
        stream
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("sending the request failed: {e}"))?;
        let mut answer = String::new();
        let read = BufReader::new(stream)
            .read_line(&mut answer)
            .await
            .map_err(|e| format!("reading the answer failed: {e}"))?;
        if read == 0 {
            return Err("the supervisor closed the connection without an answer".to_string());
        }
        serde_json::from_str(&answer).map_err(|e| format!("the answer is not JSON: {e}"))
    };
    tokio::time::timeout(within, exchange)
        .await
        .unwrap_or_else(|_| Err(format!("no answer within {within:?}")))
}

/// One request handed from a connection to the supervisor's loop.
struct Incoming {
    request: Request,
    reply: Reply,
}

/// The write half of the connection a request came on. The loop writes
/// its answer itself, so an answer it gives on its way out has reached
/// the connection before the process ends.
struct Reply(OwnedWriteHalf);

impl Reply {
    /// Write `answer` as one line and close the connection, giving up
    /// after [`STATUS_DEADLINE`] on a client that does not read.
    async fn send(self, answer: Value) {
        let mut write = self.0;
        let mut text = answer.to_string();
        text.push('\n');
        let _ = tokio::time::timeout(STATUS_DEADLINE, async {
            write.write_all(text.as_bytes()).await?;
            write.shutdown().await
        })
        .await;
    }
}

/// Accept connections on `listener`, read one request from each, and
/// hand it to the loop with the connection's write half.
fn serve_requests(listener: UnixListener, to_loop: mpsc::Sender<Incoming>) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let to_loop = to_loop.clone();
            tokio::spawn(async move {
                let (read, write) = stream.into_split();
                let mut line = String::new();
                let mut reader = BufReader::new(tokio::io::AsyncReadExt::take(read, REQUEST_CAP));
                let request = match tokio::time::timeout(
                    STATUS_DEADLINE,
                    reader.read_line(&mut line),
                )
                .await
                {
                    Ok(Ok(n)) if n > 0 => parse_request(line.trim_end()),
                    _ => return,
                };
                let _ = to_loop
                    .send(Incoming {
                        request,
                        reply: Reply(write),
                    })
                    .await;
            });
        }
    });
}

impl Session {
    /// `daemon start`: hand the start to a supervisor in the background
    /// and wait for the daemon to be ready or the start to end. The
    /// ready line is handed back on success.
    pub async fn detached_daemon_start(&mut self, start: DaemonStart) -> ToolResult<String> {
        let args = start.args();
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut call = self.begin("daemon start", shown.clone(), true).await?;
        let mut ready_line = None;
        let result = self
            .detached_start_inner(&mut call, &start, &args, &shown, &mut ready_line)
            .await;
        let outcome = self.end(call, result)?;
        ready_line.ok_or(outcome)
    }

    async fn detached_start_inner(
        &mut self,
        call: &mut Call,
        start: &DaemonStart,
        args: &[OsString],
        shown: &[String],
        ready_line: &mut Option<String>,
    ) -> ToolResult<Outcome> {
        let paths = self.paths().clone();
        self.ensure_no_writer()?;
        let stub = self.refresh_stub().await?;
        build::verify(&paths)?;
        let pre = self
            .preflight(args, None, None, start.dotenv_path(), stub.as_deref())
            .await?;

        let (n, instance) = self.new_instance(InstanceKind::Daemon)?;
        call.set_instance(n);
        let timeout = start.timeout_value();
        let intent = write_json(
            &instance.join("start.json"),
            &StartRecord {
                args: shown.to_vec(),
                dotenv: start.dotenv_path().map(|p| p.display().to_string()),
                started: now(),
                parent_pid: std::process::id(),
                hosting: Hosting::Detached,
                stub_url: stub.clone(),
                timeout_ms: Some(timeout.as_millis() as u64),
                idle_ms: start.idle_value().map(|limit| limit.as_millis() as u64),
            },
        )
        .and_then(|()| {
            write_json(
                &instance.join("plan.json"),
                &PlanFile::encode(&Plan {
                    env: pre.env.clone(),
                    cwd: pre.plan.cwd.clone(),
                    args: args.to_vec(),
                }),
            )
        });
        if let Err(e) = intent {
            let _ = write_recovered(
                &instance,
                Reason::NeverHandedOff,
                Some(format!("recording the start failed: {e}")),
            );
            return Err(Outcome::io(STAGE_INSTANCE, "record the start", e));
        }
        self.pause(PAUSE_START_AFTER_INTENT).await;

        let mut launched = self.hand_off(InstanceKind::Daemon, n, &[]).await?;
        let deadline = Instant::now() + timeout + TEARDOWN_BUDGET + CLIENT_MARGIN;
        let sock = instance.join("supervisor.sock");
        loop {
            if let Some(found) = read_end(&paths, &instance)? {
                return conclude(found, ready_line);
            }
            match launched.child.try_wait() {
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => {
                    // One more look, so a record written just before the
                    // supervisor left is not missed.
                    if let Some(found) = read_end(&paths, &instance)? {
                        return conclude(found, ready_line);
                    }
                    return Err(Outcome::tool_error(
                        STAGE_SUPERVISOR,
                        Problem::new("the daemon's supervisor ended without a record")
                            .detail(format!(
                                "supervisor pid {} left neither daemon.json nor exit.json",
                                launched.registration.pid
                            ))
                            .hint("run 'bookrack-sandbox daemon stop <dir>' to judge the instance"),
                    ));
                }
            }
            if Instant::now() >= deadline {
                let asked = ask(
                    &sock,
                    &json!({ "op": "stop", "within_ms": 0 }),
                    TEARDOWN_BUDGET + CLIENT_MARGIN,
                )
                .await;
                let exited = read_state_opt::<ExitRecord>(&paths, &instance.join("exit.json"))
                    .ok()
                    .flatten();
                call.note(match (&asked, &exited) {
                    (_, Some(_)) => "the supervisor wrote exit.json after the stop".to_string(),
                    (Err(why), None) => format!("the stop went unanswered: {why}"),
                    (Ok(_), None) => "the supervisor answered the stop without exit.json".into(),
                });
                return Err(Outcome::tool_error(
                    STAGE_SUPERVISOR,
                    Problem::new("the daemon's supervisor overran its own bound")
                        .detail(format!(
                            "neither daemon.json nor exit.json appeared within {:?} of go",
                            timeout + TEARDOWN_BUDGET + CLIENT_MARGIN
                        ))
                        .hint("run 'bookrack-sandbox daemon stop <dir>' to judge the instance"),
                ));
            }
            tokio::time::sleep(STATE_POLL).await;
        }
    }
}

/// How a start ended, as its instance files say.
enum End {
    Ready(ReadyRecord),
    Exit(ExitRecord),
}

fn read_end(paths: &super::ToolPaths, instance: &Path) -> ToolResult<Option<End>> {
    let refuse = |why: String| {
        Outcome::tool_error(
            STAGE_INSTANCE,
            Problem::new("cannot read the daemon instance").detail(why),
        )
    };
    if let Some(exit) =
        read_state_opt::<ExitRecord>(paths, &instance.join("exit.json")).map_err(refuse)?
    {
        return Ok(Some(End::Exit(exit)));
    }
    Ok(
        read_state_opt::<ReadyRecord>(paths, &instance.join("daemon.json"))
            .map_err(refuse)?
            .map(End::Ready),
    )
}

/// The start's result from how it ended: the ready line on success,
/// the exit record's outcome otherwise.
fn conclude(found: End, ready_line: &mut Option<String>) -> ToolResult<Outcome> {
    match found {
        End::Ready(ready) => {
            *ready_line = Some(ready.ready_line);
            Ok(Outcome::ok())
        }
        End::Exit(exit) => Err(exit.outcome()),
    }
}

/// `daemon supervise <dir> <n>`: the background half of a detached
/// start. Returns the process's exit code.
pub async fn supervise(dir: &Path, n: u32, opts: SessionOpts) -> i32 {
    let session = match Session::open(dir, opts) {
        Ok(session) => session,
        Err(outcome) => {
            outcome.eprint();
            return super::EXIT_TOOL;
        }
    };
    let paths = session.paths().clone();
    let instance = paths.daemon_instance(n);
    let exit = |record: &ExitRecord| {
        let _ = write_json(&instance.join("exit.json"), record);
    };

    let read = (|| -> Result<(StartRecord, Plan), String> {
        let start: StartRecord = read_state(&paths, &instance.join("start.json"))?;
        let plan: PlanFile = read_state(&paths, &instance.join("plan.json"))?;
        Ok((start, plan.decode()?))
    })();
    let (start, plan) = match read {
        Ok(read) => read,
        Err(why) => {
            exit(&handoff_exit(&why));
            return super::EXIT_TOOL;
        }
    };

    let sock = instance.join("supervisor.sock");
    let listener = match UnixListener::bind(&sock) {
        Ok(listener) => listener,
        Err(e) => {
            exit(&handoff_exit(&format!(
                "binding {} ({} bytes) failed: {e}",
                sock.display(),
                sock.as_os_str().len()
            )));
            return super::EXIT_TOOL;
        }
    };
    let (to_loop, mut requests) = mpsc::channel::<Incoming>(8);
    serve_requests(listener, to_loop);
    let nonce = detach::nonce();
    if let Err(e) = register(&instance, InstanceKind::Daemon, &nonce) {
        exit(&handoff_exit(&format!("registering failed: {e}")));
        let _ = std::fs::remove_file(&sock);
        return super::EXIT_TOOL;
    }
    let status = |phase: Phase, daemon_pid: Option<u32>| json!({ "n": n, "nonce": nonce, "phase": phase.as_str(), "daemon_pid": daemon_pid });

    // Handoff: wait for go, answering requests meanwhile.
    let go = detach::read_go();
    tokio::pin!(go);
    let handoff_until = tokio::time::sleep(HANDOFF_DEADLINE);
    tokio::pin!(handoff_until);
    loop {
        tokio::select! {
            line = &mut go => {
                let line = line.unwrap_or(detach::GoLine::Eof);
                match go_refusal(line) {
                    None => break,
                    Some(why) => {
                        exit(&handoff_exit(&why));
                        return finish(&sock);
                    }
                }
            }
            () = &mut handoff_until => {
                exit(&handoff_exit(&format!("no go within {HANDOFF_DEADLINE:?}")));
                return finish(&sock);
            }
            Some(incoming) = requests.recv() => {
                match respond(Phase::Handoff, &incoming.request) {
                    Action::Status => { incoming.reply.send(status(Phase::Handoff, None)).await; }
                    Action::Refuse(why) => { incoming.reply.send(json!({ "error": why })).await; }
                    _ => {
                        let record = handoff_exit("stopped before go");
                        exit(&record);
                        incoming.reply.send(json!({ "phase": "handoff", "exit": record })).await;
                        return finish(&sock);
                    }
                }
            }
        }
    }

    // Starting: spawn from the plan, record the pid, probe.
    if let Err(e) = write_json(&instance.join("spawn.json"), &SpawnRecord::spawning()) {
        exit(&ExitRecord::from(&Outcome::io(
            STAGE_INSTANCE,
            "write spawn.json",
            e,
        )));
        return finish(&sock);
    }
    let mut cmd = std::process::Command::new(paths.bin());
    cmd.env_clear()
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&plan.cwd)
        .args(&plan.args);
    let sinks = (|| -> io::Result<(Sink, Sink)> {
        Ok((
            Sink::File(std::fs::File::create(instance.join("stdout"))?),
            Sink::File(std::fs::File::create(instance.join("stderr"))?),
        ))
    })();
    let spawned = sinks.and_then(|(stdout, stderr)| {
        DaemonProcess::spawn_with(
            Command::from(cmd),
            SpawnOpts {
                stdin: Stdio::null(),
                capture: CaptureMode::Stream { stdout, stderr },
            },
        )
    });
    let daemon = match spawned {
        Ok(daemon) => daemon,
        Err(e) => {
            exit(&ExitRecord::from(&Outcome::io(
                STAGE_SPAWN,
                "start the daemon under test",
                e,
            )));
            return finish(&sock);
        }
    };
    session.pause(PAUSE_SUPERVISE_AFTER_SPAWN).await;
    let recorded = record_spawn(&instance, &daemon);
    let mut daemon = match finish_spawn(daemon, &instance, recorded).await {
        Ok(daemon) => daemon,
        Err(_) => return finish(&sock),
    };
    let daemon_pid = daemon.id();
    let lock = paths
        .host()
        .join("runtime")
        .join(bookrack_session::tty_lock_name());
    let timeout = Duration::from_millis(
        start
            .timeout_ms
            .unwrap_or(super::exec::DEFAULT_DAEMON_TIMEOUT.as_millis() as u64),
    );

    enum Started {
        Probed(Result<crate::child::Ready, ReadyFailure>),
        Stopped(Reply),
    }
    let started = {
        let probe = daemon.probe_ready(&lock, timeout);
        tokio::pin!(probe);
        let mut open = true;
        loop {
            tokio::select! {
                probed = &mut probe => break Started::Probed(probed),
                incoming = requests.recv(), if open => {
                    let Some(incoming) = incoming else { open = false; continue };
                    match respond(Phase::Starting, &incoming.request) {
                        Action::Status => {
                            incoming.reply.send(status(Phase::Starting, daemon_pid)).await;
                        }
                        Action::Refuse(why) => {
                            incoming.reply.send(json!({ "error": why })).await;
                        }
                        _ => break Started::Stopped(incoming.reply),
                    }
                }
            }
        }
    };
    let ready = match started {
        Started::Probed(Ok(ready)) => ready,
        Started::Probed(Err(not_ready)) => {
            let failure = daemon.give_up(not_ready).await;
            exit(&ExitRecord::from(&Outcome::from_failure(&failure)));
            return finish(&sock);
        }
        Started::Stopped(reply) => {
            let failure = daemon
                .give_up(ReadyFailure {
                    kind: FailureKind::NotReady,
                    stage: Stage::Run,
                    why: "stopped during bring-up".to_string(),
                })
                .await;
            let record = ExitRecord::from(&Outcome::from_failure(&failure)).with_phase("starting");
            exit(&record);
            reply
                .send(json!({ "phase": "starting", "exit": record }))
                .await;
            return finish(&sock);
        }
    };
    let recorded = write_json(
        &instance.join("daemon.json"),
        &ReadyRecord {
            pid: ready.pid,
            control_sock: ready.control_sock.display().to_string(),
            ready_line: ready.ready_line.clone(),
        },
    )
    .map_err(|e| Outcome::io(STAGE_INSTANCE, "write daemon.json", e));
    let mut daemon = match finish_spawn(daemon, &instance, recorded).await {
        Ok(daemon) => daemon,
        Err(_) => return finish(&sock),
    };

    // Running: watch the daemon and the sandbox; stop the daemon on
    // request, or unasked once the sandbox is idle or gone.
    let idle = start.idle_ms.map(Duration::from_millis);
    let began = SystemTime::now();
    let mut looks = tokio::time::interval(WATCH_POLL);
    looks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            incoming = requests.recv() => {
                let Some(incoming) = incoming else {
                    tokio::time::sleep(STATE_POLL).await;
                    continue;
                };
                match respond(Phase::Running, &incoming.request) {
                    Action::Status => {
                        incoming.reply.send(status(Phase::Running, daemon_pid)).await;
                    }
                    Action::Refuse(why) => {
                        incoming.reply.send(json!({ "error": why })).await;
                    }
                    Action::Stop(within) => {
                        let record =
                            ExitRecord::from(&stop_outcome(daemon.stop(within).await))
                                .with_phase("running");
                        exit(&record);
                        incoming.reply.send(json!({ "phase": "running", "exit": record })).await;
                        return finish(&sock);
                    }
                    Action::AwaitExit(within) => {
                        let record =
                            ExitRecord::from(&stop_outcome(daemon.exit_within(within).await))
                                .with_phase("running");
                        exit(&record);
                        incoming.reply.send(json!({ "phase": "running", "exit": record })).await;
                        return finish(&sock);
                    }
                    Action::AbortHandoff | Action::CancelStart => {
                        unreachable!("a running supervisor neither hands off nor starts")
                    }
                }
            }
            _ = looks.tick() => {
                if let Some(cause) = watch(&paths, &instance, began, idle, || false).cause() {
                    let record =
                        ExitRecord::from(&stop_outcome(daemon.stop(DEFAULT_STOP_TIMEOUT).await))
                            .with_phase("running")
                            .with_cause(cause);
                    exit(&record);
                    return finish(&sock);
                }
            }
            () = tokio::time::sleep(STATE_POLL) => {
                match daemon.has_exited() {
                    Ok(false) => {}
                    Ok(true) | Err(_) => {
                        // An exit noticed here happened in the running
                        // phase, whoever asked for it.
                        let outcome = match daemon.exit_within(Duration::ZERO).await {
                            Ok(exit) => Outcome::from_exit(exit.status),
                            Err(failure) => Outcome::from_failure(&failure),
                        };
                        exit(&ExitRecord::from(&outcome).with_phase("running"));
                        return finish(&sock);
                    }
                }
            }
        }
    }
}

/// The supervisor's last step after its `exit.json`: remove its socket.
fn finish(sock: &Path) -> i32 {
    let _ = std::fs::remove_file(sock);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phase_answers_every_request() {
        let stop = Request::Stop {
            within: Duration::from_secs(3),
            signal: true,
        };
        let wait = Request::Stop {
            within: Duration::from_secs(3),
            signal: false,
        };
        let unknown = Request::Unknown("x".to_string());
        let table = [
            (Phase::Handoff, &Request::Status, Action::Status),
            (Phase::Handoff, &stop, Action::AbortHandoff),
            (Phase::Handoff, &wait, Action::AbortHandoff),
            (Phase::Handoff, &unknown, Action::Refuse("x".into())),
            (Phase::Starting, &Request::Status, Action::Status),
            (Phase::Starting, &stop, Action::CancelStart),
            (Phase::Starting, &wait, Action::CancelStart),
            (Phase::Starting, &unknown, Action::Refuse("x".into())),
            (Phase::Running, &Request::Status, Action::Status),
            (Phase::Running, &stop, Action::Stop(Duration::from_secs(3))),
            (
                Phase::Running,
                &wait,
                Action::AwaitExit(Duration::from_secs(3)),
            ),
            (Phase::Running, &unknown, Action::Refuse("x".into())),
        ];
        for (phase, request, action) in table {
            assert_eq!(respond(phase, request), action, "{phase:?} {request:?}");
        }
    }

    #[test]
    fn a_request_line_is_read_strictly() {
        assert_eq!(parse_request(r#"{"op":"status"}"#), Request::Status);
        assert_eq!(
            parse_request(r#"{"op":"stop","within_ms":1500}"#),
            Request::Stop {
                within: Duration::from_millis(1500),
                signal: true,
            }
        );
        assert_eq!(
            parse_request(r#"{"op":"stop","within_ms":1500,"signal":false}"#),
            Request::Stop {
                within: Duration::from_millis(1500),
                signal: false,
            }
        );
        for bad in [
            r#"{"op":"stop"}"#,
            r#"{"op":"quit"}"#,
            r#"{"within_ms":1}"#,
            "status",
        ] {
            assert!(
                matches!(parse_request(bad), Request::Unknown(_)),
                "{bad:?} was read as a request"
            );
        }
    }

    /// The plan file keeps every byte, including ones that are not
    /// UTF-8 and the characters the escaping itself uses.
    #[test]
    fn a_plan_round_trips_every_byte() {
        let plan = Plan {
            env: vec![
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (
                    OsString::from_vec(b"K\xff\\x".to_vec()),
                    OsString::from_vec(b"v a\tl\nu\xfe e\\".to_vec()),
                ),
            ],
            cwd: PathBuf::from(OsString::from_vec(b"/tmp/c w\xd0d".to_vec())),
            args: vec![
                OsString::from("run"),
                OsString::from_vec(b"--library=\x80".to_vec()),
            ],
        };
        let file = PlanFile::encode(&plan);
        let text = serde_json::to_string(&file).expect("serialize");
        let back: PlanFile = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back.decode().expect("decode"), plan);
        let broken = PlanFile {
            cwd: "\\q".to_string(),
            ..file
        };
        assert!(broken.decode().is_err(), "a bad escape is refused");
    }
}
