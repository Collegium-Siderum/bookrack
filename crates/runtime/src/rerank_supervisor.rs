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

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bookrack_rerank::ServerHealth;
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

impl RerankSupervisorConfig {
    /// Defaults: a 60 s readiness deadline polled every 250 ms (the
    /// 0.6B model loads in seconds; the headroom is for slow disks),
    /// a 5 s TERM grace, restarts backing off from 1 s.
    pub fn new(server_bin: impl Into<PathBuf>, model_path: impl Into<PathBuf>) -> Self {
        RerankSupervisorConfig {
            server_bin: server_bin.into(),
            model_path: model_path.into(),
            ctx: None,
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
    pub async fn start(
        config: RerankSupervisorConfig,
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
        let restarts = Arc::new(AtomicU32::new(0));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let monitor = tokio::spawn(monitor_loop(
            child,
            config,
            port,
            base_url.clone(),
            Arc::clone(&state),
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

/// Reserve an OS-assigned loopback port by binding and dropping a
/// listener. The port could in principle be taken back before the
/// server binds it; on a single host that race is negligible, and a
/// lost race surfaces through the readiness deadline like any other
/// failure to come up.
fn pick_loopback_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Spawn the server process with the pinned argument set, wire its
/// log piping, and record it in the pid file.
///
/// `--embedding --pooling rank` is what the rerank endpoint requires
/// of the server and is deliberately not configurable. GPU offload is
/// left to the server's own default.
fn spawn_server(config: &RerankSupervisorConfig, port: u16) -> Result<Child, RerankerSpawnError> {
    let mut command = Command::new(&config.server_bin);
    command
        .arg("--embedding")
        .arg("--pooling")
        .arg("rank")
        .arg("-m")
        .arg(&config.model_path)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string());
    if let Some(ctx) = config.ctx {
        command.arg("-c").arg(ctx.to_string());
    }
    if let Some(threads) = config.threads {
        command.arg("--threads").arg(threads.to_string());
    }
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
    state: Arc<RwLock<SupervisorState>>,
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
                match respawn_until_ready(&config, port, &base_url, &state, &mut shutdown_rx).await
                {
                    Some(next) => child = next,
                    None => {
                        // Shutdown arrived mid-restart; nothing runs.
                        remove_pid_file(&config);
                        return;
                    }
                }
                *state.write().await = SupervisorState::Ready;
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
    state: &Arc<RwLock<SupervisorState>>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Option<Child> {
    let mut attempt = 0u32;
    loop {
        let delay = config
            .restart_backoff_base
            .saturating_mul(2u32.saturating_pow(attempt))
            .min(RESTART_BACKOFF_CAP);
        *state.write().await = SupervisorState::Restarting {
            attempt: attempt + 1,
            next_delay: delay,
        };
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown_rx.changed() => return None,
        }
        *state.write().await = SupervisorState::Starting;
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
        let err = RerankSupervisor::start(config).await.unwrap_err();
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
        let err = RerankSupervisor::start(config).await.unwrap_err();
        assert!(
            matches!(err, RerankerSpawnError::NotReadyWithinDeadline { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_executable_is_spawn_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path().join("no-such-bin"), dead_port(), dir.path());
        let err = RerankSupervisor::start(config).await.unwrap_err();
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
        let supervisor = RerankSupervisor::start(config.clone())
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
        let supervisor = RerankSupervisor::start(config.clone())
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
        let supervisor = RerankSupervisor::start(config.clone())
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
        let _ = RerankSupervisor::start(config).await;
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
        let supervisor = RerankSupervisor::start(config).await.expect("starts");
        assert!(matches!(supervisor.state().await, SupervisorState::Ready));
        supervisor.shutdown().await;
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
        let supervisor = RerankSupervisor::start(config)
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
        supervisor.shutdown().await;
    }
}
