// SPDX-License-Identifier: Apache-2.0

//! A sandbox's embed stub in a process of its own, so the commands of
//! one session reach the same stub from separate tool invocations.
//!
//! `stub start` hands a `stub serve` process the stub's width, fill and
//! identity, waits for its URL, and checks it answers the three ways a
//! daemon and the tool rely on. Every command that runs the binary under
//! test asks [`Session::refresh_stub`] for the address of this call: a
//! stub that does not answer with its own identity is refused, never
//! skipped. The stub cannot change while a daemon start has not ended,
//! since the daemon took the address when it started; for the same
//! reason an idle stub stays until no daemon start is left unfinished.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use bookrack_config::DEFAULT_EMBED_MODEL;
use bookrack_core::Problem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::detach::{
    self, HANDOFF_DEADLINE, Reason, Registration, STATE_POLL, WATCH_POLL, go_refusal, handoff_exit,
    read_state, read_state_opt, register, watch, write_recovered,
};
use super::exec::ExitRecord;
use super::recover::{EXIT_POLL, Report, StubFiles, StubProbes, StubState, judge_stub, last_ended};
use super::{
    Call, Class, InstanceKind, Outcome, PAUSE_START_AFTER_INTENT, STAGE_INSTANCE, STAGE_STUB,
    Session, SessionOpts, ToolPaths, ToolResult, now, write_atomic, write_json,
};
use crate::embed_stub::{EmbedFailure, EmbedStub, StubSpec, StubSpecError, Vectors};

/// How long `stub start` waits for the stub's URL once it said go.
// setting: internal -- how long a stub start waits for the stub's address
pub(crate) const STUB_READY: Duration = Duration::from_secs(10);

/// How long one request to a stub may take, connecting, writing and
/// reading together.
// setting: internal -- the deadline of one request to a stub, not a value to tune
pub(crate) const HTTP_DEADLINE: Duration = Duration::from_secs(5);

/// How long a stub told to shut down may take to leave.
// setting: internal -- how long a stub may take to shut down, not a value to tune
pub(crate) const STUB_STOP: Duration = Duration::from_secs(5);

/// The width `stub start` answers with unless told otherwise.
// setting: internal -- the sandbox stub's default width, overridden by --dim
pub const DEFAULT_STUB_DIM: usize = 1024;

/// The most of a stub's answer the tool reads.
// setting: internal -- bounds a stub answer the tool reads, not a value to tune
const ANSWER_CAP: usize = 64 * 1024 * 1024;

/// What a stub instance records when it starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StubStartRecord {
    pub dimension: usize,
    pub vectors: String,
    pub identity: String,
    pub started: f64,
    pub parent_pid: u32,
    /// How long the sandbox may go without a call before the stub ends
    /// unasked. Absent when it never does.
    #[serde(default)]
    pub idle_ms: Option<u64>,
}

/// The word for a fill, as the command line and the records spell it.
pub fn vectors_word(vectors: Vectors) -> &'static str {
    match vectors {
        Vectors::Constant => "constant",
        Vectors::Spread => "spread",
        Vectors::Topic => "topic",
    }
}

/// Inverse of [`vectors_word`].
pub fn parse_vectors(word: &str) -> Option<Vectors> {
    match word {
        "constant" => Some(Vectors::Constant),
        "spread" => Some(Vectors::Spread),
        "topic" => Some(Vectors::Topic),
        _ => None,
    }
}

/// One answer from a stub: its status and body.
pub(crate) struct Answer {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

/// One request to the stub at `url`, on a fresh connection, inside
/// [`HTTP_DEADLINE`].
pub(crate) async fn http(
    url: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Answer, String> {
    let addr = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url} is not a loopback http URL"))?
        .to_string();
    let exchange = async {
        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| format!("connecting to {addr} failed: {e}"))?;
        let head = format!(
            "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n",
            body.len()
        );
        let sent: io::Result<()> = async {
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(body).await
        }
        .await;
        sent.map_err(|e| format!("sending {method} {path} failed: {e}"))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("reading the status line failed: {e}"))?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| format!("no status in {line:?}"))?;
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            reader
                .read_line(&mut header)
                .await
                .map_err(|e| format!("reading a header failed: {e}"))?;
            let header = header.trim().to_ascii_lowercase();
            if header.is_empty() {
                break;
            }
            if let Some(value) = header.strip_prefix("content-length:") {
                length = value.trim().parse().map_err(|_| "a bad content-length")?;
            }
        }
        if length > ANSWER_CAP {
            return Err(format!("an answer of {length} bytes is past the cap"));
        }
        let mut body = vec![0u8; length];
        reader
            .read_exact(&mut body)
            .await
            .map_err(|e| format!("reading the body failed: {e}"))?;
        Ok(Answer { status, body })
    };
    tokio::time::timeout(HTTP_DEADLINE, exchange)
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "{method} {path} got no answer within {HTTP_DEADLINE:?}"
            ))
        })
}

/// The identity the stub at `url` reports, or why it reported none.
pub(crate) async fn identity(url: &str) -> Result<String, String> {
    let answer = http(url, "GET", "/__stub/identity", b"").await?;
    if answer.status != 200 {
        return Err(format!("the identity endpoint answered {}", answer.status));
    }
    let body: Value = serde_json::from_slice(&answer.body)
        .map_err(|e| format!("the identity is not JSON: {e}"))?;
    body.get("identity")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the stub reports no identity".to_string())
}

/// Whether anything still accepts connections at `url`.
async fn answers(url: &str) -> bool {
    match url.strip_prefix("http://") {
        Some(addr) => matches!(
            tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(addr)).await,
            Ok(Ok(_))
        ),
        None => false,
    }
}

/// The three checks a started stub passes: the model list holds the
/// default model, an embed answers at the width, and the identity is
/// the nonce.
async fn check_stub(url: &str, dimension: usize, nonce: &str) -> Result<(), String> {
    let tags = http(url, "GET", "/api/tags", b"").await?;
    let models: Value = serde_json::from_slice(&tags.body).unwrap_or(Value::Null);
    let holds_default = models["models"]
        .as_array()
        .is_some_and(|models| models.iter().any(|m| m["name"] == DEFAULT_EMBED_MODEL));
    if tags.status != 200 || !holds_default {
        return Err(format!(
            "/api/tags answered {} without {DEFAULT_EMBED_MODEL}",
            tags.status
        ));
    }
    let request = json!({ "model": DEFAULT_EMBED_MODEL, "input": ["sbx probe"] }).to_string();
    let embed = http(url, "POST", "/api/embed", request.as_bytes()).await?;
    let vectors: Value = serde_json::from_slice(&embed.body).unwrap_or(Value::Null);
    let width = vectors["embeddings"][0].as_array().map(Vec::len);
    if embed.status != 200 || width != Some(dimension) {
        return Err(format!(
            "/api/embed answered {} with width {width:?}, not {dimension}",
            embed.status
        ));
    }
    let reported = identity(url).await?;
    if reported != nonce {
        return Err(format!(
            "the stub reports identity {reported:?}, not {nonce:?}"
        ));
    }
    Ok(())
}

/// What a stub instance's files and its endpoint say.
pub(crate) struct StubView {
    pub(crate) registration: Option<Registration>,
    pub(crate) url: Option<String>,
    /// The identity the stub reported, or why it did not.
    pub(crate) identity: Result<String, String>,
}

impl StubView {
    /// The URL, when the stub answers with its own nonce.
    fn running_url(&self) -> Option<&str> {
        match (&self.registration, &self.url, &self.identity) {
            (Some(reg), Some(url), Ok(identity)) if *identity == reg.nonce => Some(url),
            _ => None,
        }
    }

    /// Why the stub is not running, for a refusal's detail.
    fn why_silent(&self, n: u32) -> String {
        match (&self.registration, &self.url, &self.identity) {
            (None, _, _) => format!("run/stub/{n} has no serve.json"),
            (_, None, _) => format!("run/stub/{n} has no url"),
            (_, Some(url), Err(why)) => format!("{url}: {why}"),
            (Some(reg), Some(url), Ok(identity)) => format!(
                "{url} reports identity {identity:?}, but serve.json records {:?}",
                reg.nonce
            ),
        }
    }
}

/// Look at stub instance `n`: its registration, its URL, and what the
/// URL answers.
pub(crate) async fn look(paths: &ToolPaths, n: u32) -> Result<StubView, String> {
    let instance = paths.stub_instance(n);
    let registration = read_state_opt::<Registration>(paths, &instance.join("serve.json"))?;
    paths.check(&instance.join("url"), super::Expect::FileOrAbsent)?;
    let url = std::fs::read_to_string(instance.join("url"))
        .ok()
        .map(|text| text.trim().to_string());
    let identity = match &url {
        Some(url) => identity(url).await,
        None => Err("no url".to_string()),
    };
    Ok(StubView {
        registration,
        url,
        identity,
    })
}

impl Session {
    /// The stub address of this call. A stub this session started in
    /// process is used as it is. Otherwise the running stub instance's
    /// URL, once it answers with its own nonce; an instance that does
    /// not is refused. No instance at all is `None`, the discard port.
    pub(crate) async fn refresh_stub(&self) -> ToolResult<Option<String>> {
        if let Some(url) = self.stub_url() {
            return Ok(Some(url.to_string()));
        }
        let unfinished = self
            .unfinished_instances(InstanceKind::Stub)
            .map_err(|e| Outcome::io(STAGE_STUB, "list the stub instances", e))?;
        let Some(&n) = unfinished.last() else {
            return Ok(None);
        };
        let view = look(self.paths(), n).await.map_err(silent_stub)?;
        match view.running_url() {
            Some(url) => Ok(Some(url.to_string())),
            None => Err(silent_stub(view.why_silent(n))),
        }
    }

    /// `stub start`: a stub in a background process, `dimension` wide,
    /// filled as `vectors` says, that ends unasked once the sandbox has
    /// gone `idle` without a call. Its URL is handed back.
    pub async fn detached_stub_start(
        &mut self,
        dimension: usize,
        vectors: Vectors,
        idle: Option<Duration>,
    ) -> ToolResult<String> {
        let args = vec![dimension.to_string(), vectors_word(vectors).to_string()];
        let mut call = self.begin("stub start", args, true).await?;
        let mut url = None;
        let result = self
            .stub_start_inner(&mut call, dimension, vectors, idle, &mut url)
            .await;
        let outcome = self.end(call, result)?;
        url.ok_or(outcome)
    }

    async fn stub_start_inner(
        &mut self,
        call: &mut Call,
        dimension: usize,
        vectors: Vectors,
        idle: Option<Duration>,
        url_out: &mut Option<String>,
    ) -> ToolResult<Outcome> {
        let list = |kind| {
            self.unfinished_instances(kind)
                .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the instances", e))
        };
        let stubs = list(InstanceKind::Stub)?;
        if !stubs.is_empty() {
            return Err(Outcome::refused(
                STAGE_INSTANCE,
                Problem::new("cannot start a stub: the sandbox's stub has not been stopped")
                    .detail(format!(
                        "run/stub/{} has no exit.json",
                        stubs[stubs.len() - 1]
                    ))
                    .hint("run 'bookrack-sandbox stub stop <dir>' first"),
            ));
        }
        refuse_while_daemon(&list(InstanceKind::Daemon)?)?;
        let nonce = detach::nonce();
        let spec = StubSpec::new(dimension, vectors, Some(nonce.clone())).map_err(|e| match e {
            StubSpecError::Dimension { got, max } => Outcome::refused(
                STAGE_STUB,
                Problem::new(format!("cannot start a stub {got} wide"))
                    .detail(format!("the width must be 1 to {max}")),
            ),
        })?;

        let (n, instance) = self.new_instance(InstanceKind::Stub)?;
        call.set_instance(n);
        if let Err(e) = write_json(
            &instance.join("start.json"),
            &StubStartRecord {
                dimension: spec.dimension(),
                vectors: vectors_word(vectors).to_string(),
                identity: nonce.clone(),
                started: now(),
                parent_pid: std::process::id(),
                idle_ms: idle.map(|limit| limit.as_millis() as u64),
            },
        ) {
            let _ = write_recovered(
                &instance,
                Reason::NeverHandedOff,
                Some(format!("recording the start failed: {e}")),
            );
            return Err(Outcome::io(STAGE_INSTANCE, "write start.json", e));
        }
        self.pause(PAUSE_START_AFTER_INTENT).await;

        let extra = [
            "--dim".into(),
            dimension.to_string().into(),
            "--vectors".into(),
            vectors_word(vectors).into(),
            "--identity".into(),
            nonce.clone().into(),
        ];
        let mut launched = self.hand_off(InstanceKind::Stub, n, &extra).await?;

        let deadline = Instant::now() + STUB_READY;
        let url = loop {
            if let Ok(text) = std::fs::read_to_string(instance.join("url")) {
                break Some(text.trim().to_string());
            }
            let gone = !matches!(launched.child.try_wait(), Ok(None));
            if gone || Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(STATE_POLL).await;
        };
        let failed = match &url {
            None => Some(format!("the stub wrote no url within {STUB_READY:?}")),
            Some(url) => check_stub(url, dimension, &nonce).await.err(),
        };
        let Some(why) = failed else {
            *url_out = url;
            return Ok(Outcome::ok());
        };

        let mut killed = false;
        if let Some(url) = &url {
            let _ = http(url, "POST", "/__stub/shutdown", nonce.as_bytes()).await;
        }
        let deadline = Instant::now()
            + if url.is_some() {
                STUB_STOP
            } else {
                Duration::ZERO
            };
        loop {
            match launched.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => tokio::time::sleep(STATE_POLL).await,
                _ => {
                    if let Some(pgid) = i32::try_from(launched.child.id())
                        .ok()
                        .and_then(rustix::process::Pid::from_raw)
                    {
                        let _ = rustix::process::kill_process_group(
                            pgid,
                            rustix::process::Signal::KILL,
                        );
                    }
                    let _ = launched.child.wait();
                    killed = true;
                    break;
                }
            }
        }
        if !super::instance_ended(&instance) {
            let reason = if killed {
                Reason::KilledByStart
            } else {
                Reason::EndedWithoutRecord
            };
            let _ = write_recovered(&instance, reason, Some(why.clone()));
        }
        Err(Outcome::refused(
            STAGE_STUB,
            Problem::new("the started stub did not answer as a stub").detail(why),
        ))
    }

    /// `stub fail <mode>`: switch the running stub's embed arm.
    pub async fn stub_fail(&mut self, mode: &str) -> ToolResult<Outcome> {
        let call = self
            .begin("stub fail", vec![mode.to_string()], true)
            .await?;
        let result = async {
            let failure: EmbedFailure = mode.parse().map_err(|why: String| {
                Outcome::refused(
                    STAGE_STUB,
                    Problem::new(format!("'{mode}' is not a failure mode"))
                        .detail(why)
                        .hint("use none, model-not-found, overloaded or unusable-vector"),
                )
            })?;
            let url = self.refresh_stub().await?.ok_or_else(|| {
                Outcome::refused(
                    STAGE_STUB,
                    Problem::new("the sandbox has no stub to switch")
                        .hint("start one with 'bookrack-sandbox stub start <dir>'"),
                )
            })?;
            let answer = http(&url, "POST", "/__stub/failure", failure.token().as_bytes())
                .await
                .map_err(silent_stub)?;
            if answer.status != 200 {
                return Err(silent_stub(format!(
                    "the failure switch answered {}",
                    answer.status
                )));
            }
            Ok(Outcome::ok())
        }
        .await;
        self.end(call, result)
    }

    /// `stub stop`: stop the running stub, or close an instance whose
    /// process is gone. With `forget`, close every unfinished instance
    /// without sending anything.
    pub async fn stub_stop(&mut self, forget: bool) -> ToolResult<Report> {
        let mut call = self
            .begin("stub stop", vec![format!("forget={forget}")], true)
            .await?;
        let mut lines = Vec::new();
        let result = self.stub_stop_inner(&mut call, forget, &mut lines).await;
        let outcome = match self.end(call, result) {
            Ok(outcome) | Err(outcome) => outcome,
        };
        Ok(Report { outcome, lines })
    }

    pub(crate) async fn stub_stop_inner(
        &self,
        call: &mut Call,
        forget: bool,
        lines: &mut Vec<String>,
    ) -> ToolResult<Outcome> {
        let paths = self.paths().clone();
        let list = |kind| {
            self.unfinished_instances(kind)
                .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the instances", e))
        };
        refuse_while_daemon(&list(InstanceKind::Daemon)?)?;
        let unfinished = list(InstanceKind::Stub)?;
        if unfinished.is_empty() {
            lines.push("nothing to stop".to_string());
            lines.push(last_ended(&paths, InstanceKind::Stub).to_string());
            return Ok(Outcome::ok());
        }
        let mut first_failure = None;
        for n in unfinished {
            call.set_instance(n);
            let instance = paths.stub_instance(n);
            let io_err = |e: io::Error| Outcome::io(STAGE_INSTANCE, "write recovered.json", e);
            if forget {
                let record = write_recovered(&instance, Reason::Forgotten, None).map_err(io_err)?;
                lines.push(json!({ "n": n, "recovered": record }).to_string());
                continue;
            }
            let (state, view) = judge_instance(&paths, n).await?;
            let result = match state {
                StubState::Running => {
                    let (Some(reg), Some(url)) = (&view.registration, &view.url) else {
                        unreachable!("a running stub has a registration and a url");
                    };
                    let _ = http(url, "POST", "/__stub/shutdown", reg.nonce.as_bytes()).await;
                    let deadline = Instant::now() + STUB_STOP;
                    loop {
                        let exited =
                            read_state_opt::<ExitRecord>(&paths, &instance.join("exit.json"))
                                .ok()
                                .flatten();
                        if let Some(exit) = exited
                            && !answers(url).await
                        {
                            lines.push(json!({ "n": n, "exit": exit }).to_string());
                            break Ok(());
                        }
                        if Instant::now() >= deadline {
                            break Err(Outcome::refused(
                                STAGE_STUB,
                                Problem::new("the stub did not shut down")
                                    .detail(format!(
                                        "stub pid {} still answers at {url} {STUB_STOP:?} after \
                                         the shutdown",
                                        reg.pid
                                    ))
                                    .hint(forget_hint("stub")),
                            ));
                        }
                        tokio::time::sleep(STATE_POLL).await;
                    }
                }
                StubState::Closing => {
                    let exit: Option<ExitRecord> =
                        read_state_opt(&paths, &instance.join("exit.json"))
                            .ok()
                            .flatten();
                    lines.push(json!({ "n": n, "exit": exit }).to_string());
                    Ok(())
                }
                StubState::NeverHandedOff => {
                    let record =
                        write_recovered(&instance, Reason::NeverHandedOff, None).map_err(io_err)?;
                    lines.push(json!({ "n": n, "recovered": record }).to_string());
                    Ok(())
                }
                StubState::Gone => {
                    let record = write_recovered(&instance, Reason::Lost, Some(view.why_silent(n)))
                        .map_err(io_err)?;
                    lines.push(json!({ "n": n, "recovered": record }).to_string());
                    Ok(())
                }
                StubState::Held { detail } => Err(Outcome::refused(
                    STAGE_STUB,
                    Problem::new("cannot tell whether the stub is gone")
                        .detail(detail)
                        .hint(forget_hint("stub")),
                )),
            };
            if let Err(outcome) = result {
                first_failure.get_or_insert(outcome);
            }
        }
        match first_failure {
            Some(outcome) => Err(outcome),
            None => Ok(Outcome::ok()),
        }
    }

    /// `stub status`: judge every unfinished stub instance without
    /// acting on any.
    pub async fn stub_status(&self) -> ToolResult<Value> {
        let call = self.begin("stub status", Vec::new(), false).await?;
        let result = async {
            let paths = self.paths().clone();
            let unfinished = self
                .unfinished_instances(InstanceKind::Stub)
                .map_err(|e| Outcome::io(STAGE_INSTANCE, "list the stub instances", e))?;
            let mut entries = Vec::new();
            for n in unfinished {
                let (state, view) = judge_instance(&paths, n).await?;
                let (word, detail) = match &state {
                    StubState::Held { detail } => ("held", Some(detail.clone())),
                    other => (other.as_str(), None),
                };
                let idle_ms = read_state_opt::<StubStartRecord>(
                    &paths,
                    &paths.stub_instance(n).join("start.json"),
                )
                .ok()
                .flatten()
                .and_then(|start| start.idle_ms);
                entries.push(json!({
                    "n": n,
                    "state": word,
                    "url": view.url,
                    "pids": { "serve": view.registration.as_ref().map(|r| r.pid) },
                    "idle_ms": idle_ms,
                    "detail": detail,
                }));
            }
            Ok(json!({
                "unfinished": entries,
                "last_ended": last_ended(&paths, InstanceKind::Stub),
            }))
        }
        .await;
        match result {
            Ok(value) => {
                self.end(call, Ok(Outcome::ok()))?;
                Ok(value)
            }
            Err(outcome) => Err(self.end(call, Err(outcome)).unwrap_err()),
        }
    }
}

/// Judge stub instance `n`, collecting what [`judge_stub`] needs.
async fn judge_instance(paths: &ToolPaths, n: u32) -> ToolResult<(StubState, StubView)> {
    let instance = paths.stub_instance(n);
    let view = look(paths, n).await.map_err(|why| {
        Outcome::tool_error(
            STAGE_INSTANCE,
            Problem::new("cannot read the stub instance").detail(why),
        )
    })?;
    let answered = view.running_url().is_some();
    let files = StubFiles {
        registered: view.registration.is_some(),
    };
    let mut probes = StubProbes {
        answered,
        exit_after_poll: false,
        serve_alive: view
            .registration
            .as_ref()
            .is_some_and(|r| detach::pid_alive(r.pid)),
        detail: view.why_silent(n),
    };
    if files.registered && !answered {
        probes.exit_after_poll = poll_exit(&instance).await;
        probes.serve_alive = view
            .registration
            .as_ref()
            .is_some_and(|r| detach::pid_alive(r.pid));
    }
    Ok((judge_stub(&files, &probes), view))
}

/// Whether `exit.json` appears under `instance` within [`EXIT_POLL`].
pub(crate) async fn poll_exit(instance: &Path) -> bool {
    let deadline = Instant::now() + EXIT_POLL;
    loop {
        if instance.join("exit.json").exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(STATE_POLL).await;
    }
}

/// The refusal for a stub that does not answer as itself.
fn silent_stub(why: String) -> Outcome {
    Outcome::refused(
        STAGE_STUB,
        Problem::new("the sandbox's stub does not answer")
            .detail(why)
            .hint(
                "run 'bookrack-sandbox stub stop <dir>', then 'bookrack-sandbox stub start <dir>'",
            ),
    )
}

/// The refusal to change the stub while a daemon start is unfinished.
fn refuse_while_daemon(daemons: &[u32]) -> ToolResult<()> {
    match daemons.last() {
        None => Ok(()),
        Some(n) => Err(Outcome::refused(
            STAGE_INSTANCE,
            Problem::new("cannot change the stub while a daemon start has not ended")
                .detail(format!(
                    "run/daemon/{n} has no exit.json; the daemon took the stub's address when it \
                     started"
                ))
                .hint(
                    "run 'bookrack-sandbox daemon stop <dir>' or 'bookrack-sandbox teardown <dir>'",
                ),
        )),
    }
}

/// The hint for an instance the tool cannot judge.
pub(crate) fn forget_hint(what: &str) -> String {
    format!(
        "confirm by hand that the processes listed are gone or not the sandbox's, then run \
         'bookrack-sandbox {what} stop <dir> --forget'"
    )
}

/// `stub serve <dir> <n>`: the background half of a stub start.
/// Returns the process's exit code.
pub async fn serve(
    dir: &Path,
    n: u32,
    dimension: usize,
    vectors: Vectors,
    identity: String,
    opts: SessionOpts,
) -> i32 {
    let session = match Session::open(dir, opts) {
        Ok(session) => session,
        Err(outcome) => {
            outcome.eprint();
            return super::EXIT_TOOL;
        }
    };
    let paths = session.paths().clone();
    let instance = paths.stub_instance(n);
    let exit = |record: &ExitRecord| {
        let _ = write_json(&instance.join("exit.json"), record);
    };
    let start = match read_state::<StubStartRecord>(&paths, &instance.join("start.json")) {
        Ok(start) => start,
        Err(why) => {
            exit(&handoff_exit(&why));
            return super::EXIT_TOOL;
        }
    };
    if let Err(e) = register(&instance, InstanceKind::Stub, &identity) {
        exit(&handoff_exit(&format!("registering failed: {e}")));
        return super::EXIT_TOOL;
    }
    let line = match tokio::time::timeout(HANDOFF_DEADLINE, detach::read_go()).await {
        Ok(Ok(line)) => line,
        Ok(Err(_)) => detach::GoLine::Eof,
        Err(_) => {
            exit(&handoff_exit(&format!("no go within {HANDOFF_DEADLINE:?}")));
            return 0;
        }
    };
    if let Some(why) = go_refusal(line) {
        exit(&handoff_exit(&why));
        return 0;
    }
    let spec = match StubSpec::new(dimension, vectors, Some(identity)) {
        Ok(spec) => spec,
        Err(e) => {
            exit(&ExitRecord::from(&Outcome::refused(
                STAGE_STUB,
                Problem::new("cannot start the stub").detail(e.to_string()),
            )));
            return super::EXIT_TOOL;
        }
    };
    let stub = EmbedStub::start_with(spec);
    if let Err(e) = write_atomic(
        &instance.join("url"),
        format!("{}\n", stub.url()).as_bytes(),
    ) {
        exit(&ExitRecord::from(&Outcome::io(
            STAGE_STUB,
            "write the url",
            e,
        )));
        return super::EXIT_TOOL;
    }
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            exit(&ExitRecord::from(&Outcome::io(
                STAGE_STUB,
                "listen for TERM",
                e,
            )));
            return super::EXIT_TOOL;
        }
    };
    let idle = start.idle_ms.map(Duration::from_millis);
    let began = SystemTime::now();
    let mut looks = tokio::time::interval(WATCH_POLL);
    looks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let daemon_unfinished = || {
        !matches!(
            session.unfinished_instances(InstanceKind::Daemon),
            Ok(unfinished) if unfinished.is_empty()
        )
    };
    let (why, cause) = loop {
        tokio::select! {
            _ = term.recv() => break ("terminated", None),
            _ = looks.tick() => {
                if let Some(cause) = watch(&paths, &instance, began, idle, daemon_unfinished).cause() {
                    break (cause, Some(cause));
                }
            }
            () = tokio::time::sleep(STATE_POLL) => {
                if stub.wait_shutdown(Duration::ZERO) {
                    break ("shutdown", None);
                }
            }
        }
    };
    exit(&ExitRecord {
        class: Class::Ok,
        stage: STAGE_STUB.to_string(),
        code: None,
        signal: None,
        summary: None,
        detail: Some(why.to_string()),
        phase: None,
        cause: cause.map(str::to_string),
        finished: now(),
    });
    0
}
