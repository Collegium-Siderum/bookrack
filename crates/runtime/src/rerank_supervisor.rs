// SPDX-License-Identifier: Apache-2.0

//! The supervised llama-server subprocess behind the reranker stage.
//!
//! When a library's effective profile enables a reranker and no
//! operator-run server is named by an override URL, the daemon owns
//! the backend process: this module spawns the pinned `llama-server`
//! on a loopback ephemeral port, holds startup until its `/health`
//! endpoint reports the model loaded, restarts it with backoff if it
//! crashes, and tears it down — TERM, then KILL — when the daemon
//! shuts down. Every state transition either succeeds or surfaces as
//! a typed error; nothing degrades silently.
//!
//! The supervisor deliberately stays minimal: one subprocess, one
//! model, a pinned argument set. It exposes the base URL for a
//! `bookrack_rerank::RerankClient` and a state snapshot for doctor;
//! the HTTP protocol itself lives in the rerank crate.
//!
//! A pid file in the runtime directory guards against orphans: the
//! supervisor records its child there, and the next start kills a
//! recorded process that is still alive and still runs llama-server —
//! the one leak `kill_on_drop` cannot cover, a daemon killed with
//! SIGKILL.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bookrack_config::llama_server_pin::locate_llama_server;
use bookrack_config::reranker_model_pin::locate_reranker_model;
use bookrack_config::{Config, RerankerConfig};
use bookrack_index_profile::RerankerKind;
use bookrack_rerank::ServerHealth;
use eyre::{Context, bail, eyre};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, watch};
use tracing::{debug, warn};

/// Filename of the orphan-guard pid file, under the runtime directory.
pub const LLAMA_SERVER_PID_FILENAME: &str = "llama-server.pid";

/// Why the supervised server could not be brought up. Startup
/// failures are daemon-startup failures: the profile promised a
/// reranker, so a backend that cannot serve refuses bring-up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RerankerSpawnError {
    /// The executable would not start at all.
    #[error("failed to spawn {}: {source}", bin.display())]
    SpawnFailed {
        bin: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The process runs but `/health` never reported ready in time.
    #[error("llama-server did not become ready within {deadline:?}")]
    NotReadyWithinDeadline { deadline: Duration },

    /// The process exited before ever becoming ready.
    #[error("llama-server exited during startup with {status}")]
    ExitedEarly { status: ExitStatus },

    /// No loopback port could be reserved for the server.
    #[error("no free loopback port: {0}")]
    NoFreePort(#[source] std::io::Error),
}

/// A snapshot of where the supervised server is in its lifecycle.
#[derive(Debug, Clone)]
pub enum SupervisorState {
    /// Spawned, model still loading; only observable during a restart
    /// window's readiness wait (initial startup blocks in
    /// [`RerankSupervisor::start`] instead).
    Starting,
    /// `/health` reports the model loaded; requests are being served.
    Ready,
    /// The server exited unexpectedly; the next respawn attempt runs
    /// after `next_delay`. Rerank calls fail as unreachable until the
    /// server is ready again.
    Restarting { attempt: u32, next_delay: Duration },
}

/// How to run and watch the server. `new` pins the defaults; the
/// remaining fields exist for the config surface (`ctx`, `threads`,
/// `pid_file`) and for tests, which shorten the clocks and fix the
/// port to one they serve themselves.
#[derive(Debug, Clone)]
pub struct RerankSupervisorConfig {
    /// The `llama-server` executable to spawn.
    pub server_bin: PathBuf,
    /// The GGUF model file to load.
    pub model_path: PathBuf,
    /// `-c` context size; the server's own default when `None`.
    pub ctx: Option<u32>,
    /// `--threads`; the server's own choice when `None`.
    pub threads: Option<u32>,
    /// Orphan-guard pid file; no guard when `None`.
    pub pid_file: Option<PathBuf>,
    /// Fixed port instead of an ephemeral one.
    pub port: Option<u16>,
    /// How long `/health` may take to first report ready.
    pub ready_timeout: Duration,
    /// Pause between `/health` polls.
    pub ready_poll_interval: Duration,
    /// Grace between TERM and KILL at shutdown.
    pub term_grace: Duration,
    /// First restart delay; doubles per failed attempt, capped.
    pub restart_backoff_base: Duration,
}

/// Longest delay between restart attempts.
const RESTART_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// `-b`/`-ub` batch sizes for the spawned server. Rerank
/// (`--pooling rank`) requires each query+document pair to fit inside
/// one physical batch, and the server rejects a pair larger than `-ub`
/// outright, failing the whole query. Chunked passages are capped at
/// 1000 characters (`ChunkParams::default` in the ingest crate), which
/// tokenizes to ~1300 tokens in the CJK worst case; 2048 covers that
/// with headroom to spare.
const SERVER_BATCH_SIZE: u32 = 2048;

/// Default `-c` context size for the spawned server, overridable
/// through `reranker.ctx`. Left unset, the server opens the model's
/// full training context and sizes its KV cache to match — gigabytes
/// for a workload that only ever holds one query+document pair per
/// slot. The server defaults to four parallel slots, so four pairs at
/// the batch cap bound the whole working set.
const DEFAULT_SERVER_CTX: u32 = 4 * SERVER_BATCH_SIZE;

impl RerankSupervisorConfig {
    /// Defaults: a 60 s readiness deadline polled every 250 ms (the
    /// 0.6B model loads in seconds; the headroom is for slow disks),
    /// a 5 s TERM grace, restarts backing off from 1 s, and a context
    /// sized to the rerank working set ([`DEFAULT_SERVER_CTX`]).
    pub fn new(server_bin: impl Into<PathBuf>, model_path: impl Into<PathBuf>) -> Self {
        RerankSupervisorConfig {
            server_bin: server_bin.into(),
            model_path: model_path.into(),
            ctx: Some(DEFAULT_SERVER_CTX),
            threads: None,
            pid_file: None,
            port: None,
            ready_timeout: Duration::from_secs(60),
            ready_poll_interval: Duration::from_millis(250),
            term_grace: Duration::from_secs(5),
            restart_backoff_base: Duration::from_secs(1),
        }
    }
}

/// Observer invoked after every [`SupervisorState`] transition, on the
/// supervision task. Kept out of [`RerankSupervisorConfig`] so the
/// config stays plain data; the daemon passes one to map transitions
/// onto its own health signals. Callbacks must be cheap and
/// non-blocking — they run inline between supervision steps.
pub type StateCallback = Arc<dyn Fn(&SupervisorState) + Send + Sync>;

/// The supervisor's state cell paired with its observer. Every
/// transition goes through [`StateSlot::set`] — store first, notify
/// second — so a callback that reads back through the supervisor sees
/// the state it was called with, and the snapshot and the observer
/// cannot drift.
#[derive(Clone)]
struct StateSlot {
    slot: Arc<RwLock<SupervisorState>>,
    on_state: Option<StateCallback>,
}

impl StateSlot {
    async fn set(&self, next: SupervisorState) {
        *self.slot.write().await = next.clone();
        if let Some(cb) = &self.on_state {
            cb(&next);
        }
    }
}

/// A running, supervised llama-server.
///
/// Constructed only through [`RerankSupervisor::start`], which returns
/// with the server ready or with a typed error. Shared behind an `Arc`
/// by the daemon; doctor reads [`RerankSupervisor::state`], the rerank
/// client construction site reads [`RerankSupervisor::base_url`].
#[derive(Debug)]
pub struct RerankSupervisor {
    base_url: String,
    state: Arc<RwLock<SupervisorState>>,
    restarts: Arc<AtomicU32>,
    shutdown_tx: watch::Sender<bool>,
    monitor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl RerankSupervisor {
    /// Spawn the server and wait until it is ready. On a readiness
    /// failure the child is killed and the pid file cleared before the
    /// error returns — a failed bring-up leaves nothing running.
    ///
    /// `on_state` observes every subsequent state transition; the
    /// initial `Ready` is reported before this returns.
    pub async fn start(
        config: RerankSupervisorConfig,
        on_state: Option<StateCallback>,
    ) -> Result<RerankSupervisor, RerankerSpawnError> {
        if let Some(pid_file) = &config.pid_file {
            kill_recorded_orphan(pid_file);
        }
        let port = match config.port {
            Some(port) => port,
            None => pick_loopback_port().map_err(RerankerSpawnError::NoFreePort)?,
        };
        let base_url = format!("http://127.0.0.1:{port}");
        let mut child = spawn_server(&config, port)?;
        if let Err(err) = await_ready(&mut child, &base_url, &config).await {
            let _ = child.start_kill();
            let _ = child.wait().await;
            remove_pid_file(&config);
            return Err(err);
        }
        let state = Arc::new(RwLock::new(SupervisorState::Ready));
        let state_slot = StateSlot {
            slot: Arc::clone(&state),
            on_state,
        };
        state_slot.set(SupervisorState::Ready).await;
        let restarts = Arc::new(AtomicU32::new(0));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let monitor = tokio::spawn(monitor_loop(
            child,
            config,
            port,
            base_url.clone(),
            state_slot,
            Arc::clone(&restarts),
            shutdown_rx,
        ));
        Ok(RerankSupervisor {
            base_url,
            state,
            restarts,
            shutdown_tx,
            monitor: Mutex::new(Some(monitor)),
        })
    }

    /// Base URL a `RerankClient` talks to. Stable across restarts —
    /// the server respawns on the same port.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// A snapshot of the current lifecycle state.
    pub async fn state(&self) -> SupervisorState {
        self.state.read().await.clone()
    }

    /// How many times the server has exited unexpectedly and been
    /// restarted over this supervisor's lifetime.
    pub fn restarts(&self) -> u32 {
        self.restarts.load(Ordering::Relaxed)
    }

    /// Stop the server — TERM, a grace period, then KILL — remove the
    /// pid file, and end supervision. Idempotent; the second call
    /// returns once the first has finished.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(handle) = self.monitor.lock().await.take() {
            let _ = handle.await;
        }
    }
}

/// The live reranker backend behind a daemon: the stage the search
/// ops apply, plus the supervised subprocess when this daemon owns
/// one. In the operator-URL mode the stage points at the operator's
/// server and `supervisor` is `None` — the two modes converge on the
/// stage, so the query path never distinguishes them.
pub struct RerankerRuntime {
    /// The supervised subprocess; `None` in the operator-URL mode.
    pub supervisor: Option<Arc<RerankSupervisor>>,
    /// Client and candidate window for the search ops.
    pub stage: bookrack_ops::RerankStage,
}

/// Per-request timeout for query-time rerank calls: generous enough
/// for a full `top_k_in` window on a cold cache, far below a hung
/// server.
const RERANK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Transport retries for one rerank call. Two quick retries ride out
/// a connection blip without stalling an interactive query through a
/// whole supervisor restart window — that window surfaces as the
/// query error the profile's atomicity demands.
const RERANK_MAX_RETRIES: u32 = 2;

/// First retry backoff for a rerank call.
const RERANK_BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Candidate-window fallbacks when a profile omits the bounds. The
/// validator requires both fields on a cross-encoder profile, so
/// these only guard a spec that bypassed validation.
const DEFAULT_TOP_K_IN: usize = 50;
const DEFAULT_TOP_K_OUT: usize = 10;

/// Bring up the reranker backend a library's effective profile
/// demands, during daemon bring-up and before the daemon serves.
///
/// Dispatch on the deployment mode: no backend at all when the
/// profile enables no reranker stage; a single hard `/health` probe
/// when `reranker.url` (or its env override) names an operator-run
/// server — the operator owns that process, so nothing is spawned and
/// no supervisor exists; otherwise the artifacts are located and a
/// supervised llama-server is spawned and held to readiness. Either
/// verified mode upholds the profile's promise at startup; every
/// failure refuses bring-up with the repair spelled out.
pub async fn bring_up_reranker(
    cfg: &Config,
    runtime_dir: &Path,
    on_state: Option<StateCallback>,
) -> eyre::Result<Option<RerankerRuntime>> {
    let Some(effective) =
        crate::profile::effective_index_profile(cfg).context("resolve the effective profile")?
    else {
        return Ok(None);
    };
    let spec = &effective.profile.reranker;
    if spec.kind == RerankerKind::None {
        return Ok(None);
    }
    let reranker_cfg = RerankerConfig::resolve(cfg.root_config());
    bring_up_backend(
        &effective.profile.name,
        spec.model.as_deref(),
        spec.top_k_in
            .map(|k| k as usize)
            .unwrap_or(DEFAULT_TOP_K_IN),
        spec.top_k_out
            .map(|k| k as usize)
            .unwrap_or(DEFAULT_TOP_K_OUT),
        reranker_cfg,
        runtime_dir,
        on_state,
    )
    .await
    .map(Some)
}

/// The mode dispatch behind [`bring_up_reranker`], after the profile
/// has resolved and demanded a reranker: operator URL → one hard
/// probe, otherwise locate the artifacts and supervise. Factored out
/// so the dispatch is testable without an effective profile.
async fn bring_up_backend(
    profile: &str,
    model_tag: Option<&str>,
    top_k_in: usize,
    top_k_out: usize,
    reranker_cfg: RerankerConfig,
    runtime_dir: &Path,
    on_state: Option<StateCallback>,
) -> eyre::Result<RerankerRuntime> {
    let model_tag = model_tag
        .ok_or_else(|| eyre!("profile '{profile}' enables a reranker but names no model"))?;
    if let Some(url) = reranker_cfg.url {
        match bookrack_rerank::probe_health(&url).await {
            ServerHealth::Ready => Ok(RerankerRuntime {
                supervisor: None,
                stage: rerank_stage(&url, model_tag, top_k_in, top_k_out)?,
            }),
            not_ready => bail!(
                "the rerank server at {url} is not ready ({detail}); profile '{profile}' \
                 promises a reranker stage. Fix the server behind reranker.url, or switch \
                 to a profile without a reranker stage.",
                detail = match &not_ready {
                    ServerHealth::Starting => "still loading its model".to_string(),
                    ServerHealth::Unreachable(detail) => detail.clone(),
                    ServerHealth::Ready => unreachable!("matched above"),
                },
            ),
        }
    } else {
        let bin = locate_llama_server().path.ok_or_else(|| {
            eyre!(
                "profile '{profile}' promises a reranker stage but no llama-server binary \
                 is installed; run `bookrack doctor --install-reranker`"
            )
        })?;
        let model = locate_reranker_model(model_tag).path.ok_or_else(|| {
            eyre!(
                "profile '{profile}' promises reranker model '{model_tag}' but no model \
                 file is installed; run `bookrack doctor --install-reranker`"
            )
        })?;
        let mut config = RerankSupervisorConfig::new(bin, model);
        config.ctx = reranker_cfg.ctx.or(config.ctx);
        config.threads = reranker_cfg.threads;
        config.pid_file = Some(runtime_dir.join(LLAMA_SERVER_PID_FILENAME));
        let supervisor = RerankSupervisor::start(config, on_state)
            .await
            .context("bring up the supervised llama-server")?;
        let stage = rerank_stage(supervisor.base_url(), model_tag, top_k_in, top_k_out)?;
        Ok(RerankerRuntime {
            supervisor: Some(Arc::new(supervisor)),
            stage,
        })
    }
}

/// Build the search ops' stage: the rerank client pointed at the
/// backend the mode dispatch selected, with the query-time transport
/// policy pinned here.
fn rerank_stage(
    base_url: &str,
    model_tag: &str,
    top_k_in: usize,
    top_k_out: usize,
) -> eyre::Result<bookrack_ops::RerankStage> {
    let client = bookrack_rerank::RerankClient::new(
        base_url,
        model_tag,
        RERANK_REQUEST_TIMEOUT,
        RERANK_MAX_RETRIES,
        RERANK_BACKOFF_BASE,
    )
    .map_err(|e| eyre!("build the rerank client: {e}"))?;
    Ok(bookrack_ops::RerankStage {
        client: Arc::new(client),
        top_k_in,
        top_k_out,
    })
}

/// Reserve an OS-assigned loopback port by binding and dropping a
/// listener. The port could in principle be taken back before the
/// server binds it; on a single host that race is negligible, and a
/// lost race surfaces through the readiness deadline like any other
/// failure to come up.
fn pick_loopback_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// The spawned server's full argument list.
///
/// `--embedding --pooling rank` is what the rerank endpoint requires
/// of the server and is deliberately not configurable. The batch
/// sizes ([`SERVER_BATCH_SIZE`]) guarantee any query+document pair
/// fits one physical batch. `-ngl 99` offloads every layer to the GPU
/// when the build has one and falls back to CPU cleanly when not.
/// Slot reuse by prompt similarity is disabled: rerank prompts share
/// almost no prefix, so scanning slot caches for one only adds
/// per-request latency that grows with the number of slots touched.
fn server_args(config: &RerankSupervisorConfig, port: u16) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--embedding".into(),
        "--pooling".into(),
        "rank".into(),
        "-m".into(),
        config.model_path.clone().into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
        "-ub".into(),
        SERVER_BATCH_SIZE.to_string().into(),
        "-b".into(),
        SERVER_BATCH_SIZE.to_string().into(),
        "-ngl".into(),
        "99".into(),
        "--slot-prompt-similarity".into(),
        "0".into(),
    ];
    if let Some(ctx) = config.ctx {
        args.push("-c".into());
        args.push(ctx.to_string().into());
    }
    if let Some(threads) = config.threads {
        args.push("--threads".into());
        args.push(threads.to_string().into());
    }
    args
}

/// Spawn the server process with the pinned argument set, wire its
/// log piping, and record it in the pid file.
fn spawn_server(config: &RerankSupervisorConfig, port: u16) -> Result<Child, RerankerSpawnError> {
    let mut command = Command::new(&config.server_bin);
    command.args(server_args(config, port));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The backstop for a panicking or early-returning daemon; the
        // orderly path is the TERM-then-KILL sequence in the monitor.
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|source| RerankerSpawnError::SpawnFailed {
            bin: config.server_bin.clone(),
            source,
        })?;
    pipe_logs(&mut child);
    if let (Some(pid_file), Some(pid)) = (&config.pid_file, child.id()) {
        write_pid_file(pid_file, pid);
    }
    Ok(child)
}

/// Forward the child's output into tracing under the `llama_server`
/// target — stdout as debug (normal noise), stderr as warn — so the
/// operator's log directive controls its visibility.
fn pipe_logs(child: &mut Child) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(target: "llama_server", "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!(target: "llama_server", "{line}");
            }
        });
    }
}

/// Poll `/health` until the server reports ready, the deadline
/// passes, or the process exits.
async fn await_ready(
    child: &mut Child,
    base_url: &str,
    config: &RerankSupervisorConfig,
) -> Result<(), RerankerSpawnError> {
    let deadline = tokio::time::Instant::now() + config.ready_timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(RerankerSpawnError::ExitedEarly { status });
        }
        if let ServerHealth::Ready =
            bookrack_rerank::probe_health_with_timeout(base_url, config.ready_poll_interval).await
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RerankerSpawnError::NotReadyWithinDeadline {
                deadline: config.ready_timeout,
            });
        }
        tokio::time::sleep(config.ready_poll_interval).await;
    }
}

/// Watch the child until shutdown. An unexpected exit logs, flips the
/// state to `Restarting`, and respawns with exponential backoff —
/// without an attempt cap: a resident daemon has no better move than
/// to keep trying, and doctor surfaces the restart count.
async fn monitor_loop(
    mut child: Child,
    config: RerankSupervisorConfig,
    port: u16,
    base_url: String,
    state: StateSlot,
    restarts: Arc<AtomicU32>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|e| format!("unknown status ({e})"));
                warn!(
                    target: "llama_server",
                    "llama-server exited unexpectedly ({status}); restarting"
                );
                restarts.fetch_add(1, Ordering::Relaxed);
                match respawn_until_ready(&config, port, &base_url, &state, &mut shutdown_rx)
                    .await
                {
                    Some(next) => child = next,
                    None => {
                        // Shutdown arrived mid-restart; nothing runs.
                        remove_pid_file(&config);
                        return;
                    }
                }
                state.set(SupervisorState::Ready).await;
            }
            _ = shutdown_rx.changed() => {
                stop_child(&mut child, &config).await;
                remove_pid_file(&config);
                return;
            }
        }
    }
}

/// Back off, respawn, and wait for readiness, repeating on failure
/// until a server is ready (returned) or shutdown is requested
/// (`None`). No attempt cap; the delay doubles up to the cap.
async fn respawn_until_ready(
    config: &RerankSupervisorConfig,
    port: u16,
    base_url: &str,
    state: &StateSlot,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Option<Child> {
    let mut attempt = 0u32;
    loop {
        let delay = config
            .restart_backoff_base
            .saturating_mul(2u32.saturating_pow(attempt))
            .min(RESTART_BACKOFF_CAP);
        state
            .set(SupervisorState::Restarting {
                attempt: attempt + 1,
                next_delay: delay,
            })
            .await;
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown_rx.changed() => return None,
        }
        state.set(SupervisorState::Starting).await;
        match spawn_server(config, port) {
            Ok(mut child) => {
                tokio::select! {
                    ready = await_ready(&mut child, base_url, config) => match ready {
                        Ok(()) => return Some(child),
                        Err(err) => {
                            warn!(
                                target: "llama_server",
                                "restarted llama-server failed to become ready: {err}"
                            );
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                        }
                    },
                    _ = shutdown_rx.changed() => {
                        stop_child(&mut child, config).await;
                        return None;
                    }
                }
            }
            Err(err) => {
                warn!(target: "llama_server", "llama-server respawn failed: {err}");
            }
        }
        attempt += 1;
    }
}

/// Stop the child: TERM, wait out the grace period, then KILL. On
/// non-unix targets TERM has no equivalent, so KILL is immediate.
async fn stop_child(child: &mut Child, config: &RerankSupervisorConfig) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        send_sigterm(pid);
        if tokio::time::timeout(config.term_grace, child.wait())
            .await
            .is_ok()
        {
            return;
        }
        warn!(
            target: "llama_server",
            "llama-server ignored TERM for {:?}; killing", config.term_grace
        );
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Record the child in the orphan-guard file: pid on the first line,
/// the start time (unix seconds, informational) on the second.
fn write_pid_file(path: &Path, pid: u32) {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Err(err) = std::fs::write(path, format!("{pid}\n{started}\n")) {
        warn!("could not write {}: {err}", path.display());
    }
}

/// Remove the orphan-guard file after an orderly stop.
fn remove_pid_file(config: &RerankSupervisorConfig) {
    if let Some(path) = &config.pid_file {
        let _ = std::fs::remove_file(path);
    }
}

/// Kill the process a stale pid file records, provided it is still
/// alive and still runs llama-server. Better to kill and respawn a
/// managed process too eagerly than to leave two servers running; a
/// pid that was recycled by an unrelated program fails the name check
/// and is left alone. The stale file is removed either way.
fn kill_recorded_orphan(pid_file: &Path) {
    let Ok(content) = std::fs::read_to_string(pid_file) else {
        return;
    };
    let recorded = content
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u32>().ok());
    if let Some(pid) = recorded {
        #[cfg(unix)]
        if process_is_alive(pid) && command_line_names_llama_server(pid) {
            warn!("killing orphaned llama-server (pid {pid}) left by a previous daemon");
            send_sigkill(pid);
        }
    }
    let _ = std::fs::remove_file(pid_file);
}

#[cfg(unix)]
fn unix_pid(pid: u32) -> Option<rustix::process::Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
}

/// Signal 0: existence probe, no signal delivered.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    unix_pid(pid).is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

#[cfg(unix)]
fn send_sigterm(pid: u32) {
    if let Some(pid) = unix_pid(pid) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

#[cfg(unix)]
fn send_sigkill(pid: u32) {
    if let Some(pid) = unix_pid(pid) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
}

/// Whether the process's command line mentions llama-server — the
/// guard that keeps the orphan kill away from a recycled pid. `ps` is
/// asked rather than a platform procfs read so macOS and Linux share
/// one path.
#[cfg(unix)]
fn command_line_names_llama_server(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("llama-server"))
        .unwrap_or(false)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Write an executable shell script fixture and return its path.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        path
    }

    /// Serve `200 {"status":"ok"}` to every connection on an owned
    /// port, standing in for a ready llama-server. The fake child
    /// never listens; the supervisor's probes hit this stub because
    /// the config pins the port.
    async fn health_stub() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut scratch = [0u8; 4096];
                    let _ = socket.read(&mut scratch).await;
                    let body = r#"{"status":"ok"}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        port
    }

    /// A port with nothing listening on it.
    fn dead_port() -> u16 {
        pick_loopback_port().expect("port")
    }

    /// A config with test-speed clocks, a pinned port, and a pid file
    /// in `dir`.
    fn test_config(bin: PathBuf, port: u16, dir: &Path) -> RerankSupervisorConfig {
        let mut config = RerankSupervisorConfig::new(bin, dir.join("model.gguf"));
        config.port = Some(port);
        config.pid_file = Some(dir.join(LLAMA_SERVER_PID_FILENAME));
        config.ready_timeout = Duration::from_secs(5);
        config.ready_poll_interval = Duration::from_millis(25);
        config.term_grace = Duration::from_millis(300);
        config.restart_backoff_base = Duration::from_millis(30);
        config
    }

    /// The pinned deployment argument set: pair-sized batches, GPU
    /// offload, slot-similarity reuse off, and a workload-sized
    /// default context that an operator `reranker.ctx` override
    /// replaces.
    #[test]
    fn server_args_pin_the_deployment_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let has_pair = |config: &RerankSupervisorConfig, name: &str, value: &str| {
            let args = server_args(config, 8080);
            args.windows(2)
                .any(|w| w[0].to_string_lossy() == name && w[1].to_string_lossy() == value)
        };
        let mut config = test_config(PathBuf::from("llama-server"), 8080, dir.path());
        assert!(has_pair(&config, "-ub", "2048"));
        assert!(has_pair(&config, "-b", "2048"));
        assert!(has_pair(&config, "-ngl", "99"));
        assert!(has_pair(&config, "--slot-prompt-similarity", "0"));
        assert!(has_pair(&config, "-c", "8192"));
        config.ctx = Some(4096);
        assert!(has_pair(&config, "-c", "4096"));
    }

    fn recorded_pid(config: &RerankSupervisorConfig) -> u32 {
        let path = config.pid_file.as_ref().expect("pid file configured");
        std::fs::read_to_string(path)
            .expect("pid file exists")
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .expect("pid recorded")
    }

    async fn wait_until(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
        let end = tokio::time::Instant::now() + deadline;
        while tokio::time::Instant::now() < end {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_child_that_exits_at_once_is_exited_early() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = script(dir.path(), "fake-server", "#!/bin/sh\nexit 3\n");
        let config = test_config(bin, dead_port(), dir.path());
        let err = RerankSupervisor::start(config, None).await.unwrap_err();
        assert!(
            matches!(err, RerankerSpawnError::ExitedEarly { .. }),
            "got {err:?}"
        );
        assert!(
            !dir.path().join(LLAMA_SERVER_PID_FILENAME).exists(),
            "a failed bring-up clears the pid file"
        );
    }

    #[tokio::test]
    async fn a_child_that_never_serves_times_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = script(dir.path(), "fake-server", "#!/bin/sh\nsleep 60\n");
        let mut config = test_config(bin, dead_port(), dir.path());
        config.ready_timeout = Duration::from_millis(300);
        let err = RerankSupervisor::start(config, None).await.unwrap_err();
        assert!(
            matches!(err, RerankerSpawnError::NotReadyWithinDeadline { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_executable_is_spawn_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path().join("no-such-bin"), dead_port(), dir.path());
        let err = RerankSupervisor::start(config, None).await.unwrap_err();
        assert!(
            matches!(err, RerankerSpawnError::SpawnFailed { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn ready_then_shutdown_stops_the_child_and_clears_the_pid_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = script(dir.path(), "fake-server", "#!/bin/sh\nsleep 60\n");
        let port = health_stub().await;
        let config = test_config(bin, port, dir.path());
        let pid_path = config.pid_file.clone().expect("pid file");
        let supervisor = RerankSupervisor::start(config.clone(), None)
            .await
            .expect("starts");
        assert!(supervisor.base_url().ends_with(&port.to_string()));
        assert!(matches!(supervisor.state().await, SupervisorState::Ready));
        let pid = recorded_pid(&config);
        assert!(process_is_alive(pid));
        supervisor.shutdown().await;
        assert!(!pid_path.exists(), "pid file removed on orderly stop");
        assert!(
            wait_until(Duration::from_secs(2), || !process_is_alive(pid)).await,
            "child stopped"
        );
    }

    #[tokio::test]
    async fn a_killed_child_is_restarted_to_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = script(dir.path(), "fake-server", "#!/bin/sh\nsleep 60\n");
        let port = health_stub().await;
        let config = test_config(bin, port, dir.path());
        let observed: Arc<std::sync::Mutex<Vec<SupervisorState>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_by_cb = Arc::clone(&observed);
        let supervisor = RerankSupervisor::start(
            config.clone(),
            Some(Arc::new(move |state: &SupervisorState| {
                observed_by_cb.lock().unwrap().push(state.clone());
            })),
        )
        .await
        .expect("starts");
        let first_pid = recorded_pid(&config);
        send_sigkill(first_pid);
        let recovered = wait_until(Duration::from_secs(5), || {
            supervisor.restarts() >= 1 && recorded_pid(&config) != first_pid
        })
        .await;
        assert!(recovered, "supervisor respawned after a crash");
        assert!(
            wait_until(Duration::from_secs(5), || {
                matches!(
                    futures_ready_state(&supervisor),
                    Some(SupervisorState::Ready)
                )
            })
            .await,
            "respawned server reaches Ready"
        );
        supervisor.shutdown().await;

        let seen = observed.lock().unwrap().clone();
        assert!(
            matches!(seen.first(), Some(SupervisorState::Ready)),
            "callback reports the initial Ready: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|s| matches!(s, SupervisorState::Restarting { .. })),
            "callback observes the restart transition: {seen:?}"
        );
        assert!(
            matches!(seen.last(), Some(SupervisorState::Ready)),
            "callback observes the recovery to Ready: {seen:?}"
        );
    }

    /// A non-async peek at the state for use inside `wait_until`'s
    /// sync closure: `try_read` only fails under writer contention,
    /// in which case the poll simply tries again.
    fn futures_ready_state(supervisor: &RerankSupervisor) -> Option<SupervisorState> {
        supervisor.state.try_read().ok().map(|s| s.clone())
    }

    #[tokio::test]
    async fn a_term_ignoring_child_is_killed_after_the_grace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = script(
            dir.path(),
            "fake-server",
            "#!/bin/sh\ntrap '' TERM\nwhile :; do sleep 0.2; done\n",
        );
        let port = health_stub().await;
        let config = test_config(bin, port, dir.path());
        let supervisor = RerankSupervisor::start(config.clone(), None)
            .await
            .expect("starts");
        let pid = recorded_pid(&config);
        supervisor.shutdown().await;
        assert!(
            wait_until(Duration::from_secs(2), || !process_is_alive(pid)).await,
            "TERM-ignoring child was killed"
        );
    }

    #[tokio::test]
    async fn a_recorded_orphan_is_killed_before_the_next_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The orphan's command line must name llama-server, as a real
        // leftover's would.
        let orphan_bin = script(dir.path(), "llama-server", "#!/bin/sh\nsleep 60\n");
        let mut orphan = std::process::Command::new(&orphan_bin)
            .spawn()
            .expect("spawn orphan");
        let orphan_pid = orphan.id();
        let pid_path = dir.path().join(LLAMA_SERVER_PID_FILENAME);
        std::fs::write(&pid_path, format!("{orphan_pid}\n0\n")).expect("write pid file");

        let bin = script(dir.path(), "fake-server", "#!/bin/sh\nexit 0\n");
        let config = test_config(bin, dead_port(), dir.path());
        let _ = RerankSupervisor::start(config, None).await;
        // The test holds the orphan's handle, so the killed process
        // stays a zombie until reaped: `try_wait` is the liveness
        // check, not signal 0.
        assert!(
            wait_until(Duration::from_secs(2), || {
                orphan.try_wait().expect("query orphan").is_some()
            })
            .await,
            "orphan was killed"
        );
    }

    #[tokio::test]
    async fn a_stale_pid_file_with_a_dead_pid_is_tolerated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = script(dir.path(), "fake-server", "#!/bin/sh\nsleep 60\n");
        let port = health_stub().await;
        let config = test_config(bin, port, dir.path());
        let pid_path = config.pid_file.clone().expect("pid file");
        // A pid far above any live process on the test host.
        std::fs::write(&pid_path, "99999999\n0\n").expect("write pid file");
        let supervisor = RerankSupervisor::start(config, None).await.expect("starts");
        assert!(matches!(supervisor.state().await, SupervisorState::Ready));
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn bring_up_backend_with_a_ready_url_probes_and_spawns_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let port = health_stub().await;
        let cfg = RerankerConfig {
            url: Some(format!("http://127.0.0.1:{port}")),
            ..RerankerConfig::default()
        };
        let backend = bring_up_backend(
            "p",
            Some("Qwen3-Reranker-0.6B"),
            50,
            10,
            cfg,
            dir.path(),
            None,
        )
        .await
        .expect("operator mode brings up");
        assert!(
            backend.supervisor.is_none(),
            "operator mode owns no supervisor"
        );
        assert_eq!(backend.stage.top_k_in, 50);
        assert_eq!(backend.stage.top_k_out, 10);
        assert!(
            !dir.path().join(LLAMA_SERVER_PID_FILENAME).exists(),
            "operator mode writes no pid file"
        );
    }

    #[tokio::test]
    async fn bring_up_backend_with_a_dead_url_refuses_with_the_repair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = RerankerConfig {
            url: Some(format!("http://127.0.0.1:{}", dead_port())),
            ..RerankerConfig::default()
        };
        let err = bring_up_backend(
            "quality",
            Some("Qwen3-Reranker-0.6B"),
            50,
            10,
            cfg,
            dir.path(),
            None,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("quality"), "{text}");
        assert!(text.contains("reranker.url"), "{text}");
    }

    #[tokio::test]
    async fn bring_up_backend_without_a_model_tag_refuses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = bring_up_backend(
            "p",
            None,
            50,
            10,
            RerankerConfig::default(),
            dir.path(),
            None,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        assert!(err.to_string().contains("names no model"), "{err}");
    }

    /// The supervised arm of the dispatch against the real pinned
    /// artifacts; skips cleanly where they are not installed.
    #[tokio::test]
    async fn bring_up_backend_supervises_the_real_server_when_installed() {
        if locate_llama_server().path.is_none()
            || locate_reranker_model("Qwen3-Reranker-0.6B").path.is_none()
        {
            eprintln!("skipping: reranker artifacts not installed");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = bring_up_backend(
            "p",
            Some("Qwen3-Reranker-0.6B"),
            50,
            10,
            RerankerConfig::default(),
            dir.path(),
            None,
        )
        .await
        .expect("supervised mode brings up");
        let supervisor = backend
            .supervisor
            .expect("supervised mode holds a supervisor");
        assert!(matches!(supervisor.state().await, SupervisorState::Ready));
        assert!(dir.path().join(LLAMA_SERVER_PID_FILENAME).exists());
        // The stage's client points at the supervised server; a real
        // call through it proves the two halves are wired together.
        let ranked = backend
            .stage
            .client
            .rerank(
                "What animal is a panda?",
                &["Paris is the capital of France.".to_string()],
                1,
            )
            .await
            .expect("the stage client reaches the supervised server");
        assert_eq!(ranked.len(), 1);
        supervisor.shutdown().await;
        assert!(!dir.path().join(LLAMA_SERVER_PID_FILENAME).exists());
    }

    /// End-to-end against the real pinned artifacts. Skips cleanly on
    /// hosts (CI among them) where `doctor --install-reranker` has not
    /// run, mirroring how PDF tests skip without a PDFium library.
    #[tokio::test]
    async fn real_llama_server_becomes_ready_and_reranks() {
        let Some(bin) = bookrack_config::llama_server_pin::locate_llama_server().path else {
            eprintln!("skipping: no llama-server binary installed");
            return;
        };
        let Some(model) =
            bookrack_config::reranker_model_pin::locate_reranker_model("Qwen3-Reranker-0.6B").path
        else {
            eprintln!("skipping: no reranker model installed");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = RerankSupervisorConfig::new(bin, model);
        config.pid_file = Some(dir.path().join(LLAMA_SERVER_PID_FILENAME));
        let supervisor = RerankSupervisor::start(config, None)
            .await
            .expect("real server ready");
        let client = bookrack_rerank::RerankClient::new(
            supervisor.base_url(),
            "Qwen3-Reranker-0.6B",
            Duration::from_secs(30),
            0,
            Duration::from_millis(1),
        )
        .expect("client builds");
        let documents = vec![
            "Paris is the capital of France.".to_string(),
            "The giant panda is a bear species endemic to China.".to_string(),
        ];
        let ranked = client
            .rerank("What animal is a panda?", &documents, 2)
            .await
            .expect("rerank succeeds");
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].index, 1, "the on-topic passage ranks first");
        assert!(ranked[0].score > ranked[1].score);
        // A pair well past the server's own 512-token physical-batch
        // default; the pinned `-ub` must keep it processable instead
        // of failing the whole request as too large.
        let long_document = "the quick brown fox jumps over the lazy dog ".repeat(80);
        let ranked = client
            .rerank("does the fox jump over the dog?", &[long_document], 1)
            .await
            .expect("a long document reranks");
        assert_eq!(ranked.len(), 1);
        supervisor.shutdown().await;
    }
}
