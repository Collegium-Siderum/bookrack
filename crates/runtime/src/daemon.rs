// SPDX-License-Identifier: Apache-2.0

//! Daemon-side process runtime shared by `bookrack run` and the
//! headless `bookrack-mcp` binary.
//!
//! [`DaemonRuntime::start`] performs the fixed eleven-step bring-up:
//! resolve the runtime directory, acquire the session [`TtyLock`],
//! initialise observability, build the embedding client, preflight the
//! catalog schema, open the query [`Library`], wrap it in an [`Ops`],
//! warm a one-handle [`LibraryRegistry`], install the platform signal
//! aggregator, load the persistent ingest queue state, and — in the
//! `bookrack run` profile — spawn the queue worker.
//!
//! Callers wire the MCP listener and any REPL surface as separate
//! [`tokio::task::JoinHandle`]s and hand them to
//! [`DaemonRuntime::run_until_shutdown`], which joins each one through
//! the shared `broadcast::Sender<()>`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, EmbedConfig, LibrarySelection, LogConfig, McpConfig, ResolutionSource, SearchConfig,
};
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_glean::GleanParams;
use bookrack_ingest::IngestParams;
use bookrack_obs::stream::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops, PapersPaths};
use bookrack_query::Library;
use bookrack_session::{TtyLock, resolve_runtime_dir, tty_lock_name};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::audit_helpers::{
    load_audit_data, load_audit_profile, load_heading_patterns, load_paper_audit_data,
    load_paper_audit_profile,
};
use crate::control::events::{
    DEFAULT_EVENT_CHANNEL_CAPACITY, DaemonState, DaemonStateFlag, Event, EventStreamHandle, Stage,
};
use crate::control::methods::MethodContext;
use crate::control::progress::{EventProgressSink, ProgressSink};
use crate::control::socket::{
    ControlSocketGuard, ControlSocketPath, bind as bind_control_socket, run_accept_loop,
};
use crate::queue;

/// Origin of a daemon bring-up, threaded into the second-launch
/// handler so a CLI entry surfaces the recorded address while a GUI
/// entry routes a `tray.focus` RPC at the live daemon before exiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaunchMode {
    /// `bookrack run` or `bookrack-mcp`; a second launch prints the
    /// recorded pid and control socket and exits zero.
    #[default]
    Cli,
    /// Tauri-driven GUI entry; a second launch raises the existing
    /// window through `tray.focus` and exits zero. Reserved for the
    /// GUI Phase.
    Gui,
}

/// Caller-facing bring-up options. Public fields keep construction
/// declarative; helper constructors capture the two stable entry-point
/// profiles.
pub struct RuntimeOpts {
    /// Library selection forwarded to [`Config::resolve`].
    pub selection: LibrarySelection,
    /// Override the runtime directory. Falls back to `BOOKRACK_RUNTIME_DIR`
    /// or the platform default.
    pub runtime_dir: Option<PathBuf>,
    /// Override the MCP listener address; falls back to [`McpConfig::from_env`].
    pub mcp_addr: Option<SocketAddr>,
    /// Skip binding the MCP listener. The daemon still acquires the
    /// tty lock and opens the registry.
    pub no_mcp: bool,
    /// Spawn the persistent ingest queue worker. `false` for the
    /// headless `bookrack-mcp` profile, which exposes the queue state
    /// as inert zero counts.
    pub spawn_queue_worker: bool,
    /// Observability layer configuration. The daemon-REPL profile uses
    /// [`LogConfig::from_env`]; the headless profile uses
    /// [`LogConfig::for_headless_daemon`].
    pub log_config: LogConfig,
    /// [`Caller`] baked into the shared [`Ops`]: the attribution for
    /// writes that arrive without a task-scope override, i.e. through
    /// the control socket. Tool calls arriving over MCP are attributed
    /// per-call to `Caller::mcp()` by the MCP server's dispatch wrap
    /// and never read this value.
    pub caller: Caller,
    /// MCP tool surface, as published by the live `BookrackServer`.
    /// Empty for entry points that do not bring up the MCP listener;
    /// otherwise populated by the caller from
    /// `bookrack_mcp::list_tools()` so the control-plane
    /// `daemon.mcp_tools` method can answer without spinning up an
    /// MCP transport.
    pub mcp_tools: Vec<crate::control::methods::meta::McpToolInfo>,
    /// Origin of this daemon instance, threaded through to the
    /// second-launch handler so the CLI prints the recorded address
    /// while a GUI entry would instead route a `tray.focus` RPC at
    /// the live daemon. Today every entry point passes
    /// [`LaunchMode::Cli`]; the GUI entry that wires
    /// [`LaunchMode::Gui`] arrives with the Tauri tray Phase.
    pub launch_mode: LaunchMode,
}

impl RuntimeOpts {
    /// Headless `bookrack-mcp` profile: env-derived MCP address, no
    /// queue worker, file-mirrored stderr console layer.
    pub fn headless(data_dir: Option<PathBuf>, library: Option<String>) -> Self {
        Self {
            selection: LibrarySelection { data_dir, library },
            runtime_dir: None,
            mcp_addr: None,
            no_mcp: false,
            spawn_queue_worker: false,
            log_config: LogConfig::for_headless_daemon(),
            caller: Caller::cli(),
            mcp_tools: Vec::new(),
            launch_mode: LaunchMode::Cli,
        }
    }
}

/// Shared daemon state. `bookrack run` and `bookrack-mcp` both build
/// one and hand it to the MCP listener, the REPL, and the queue worker
/// as a single source of truth for the open library registry, the
/// broadcast shutdown channel, and the on-disk queue state.
pub struct DaemonRuntime {
    pub cfg: Arc<Config>,
    pub registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    pub info_context: LibraryInfoContext,
    pub log_stream: LogStreamHandle,
    pub queue_state: Arc<Mutex<QueueState>>,
    pub queue_state_path: PathBuf,
    pub queue_params_template: IngestParams,
    pub shutdown_tx: broadcast::Sender<()>,
    pub runtime_dir: PathBuf,
    pub lock_path: PathBuf,
    pub started_at: Instant,
    /// Wall-clock instant the daemon entered service. Reported to
    /// control-plane clients through `daemon.version` so they can
    /// derive an uptime.
    pub started_at_wall: DateTime<Utc>,
    pub mcp_label: String,
    /// Broadcast handle for control-plane events.
    pub event_stream: EventStreamHandle,
    /// Process-wide write mutex held by every control-plane write
    /// handler for the duration of its underlying business call.
    /// Distinct from the cross-process [`TtyLock`]: this serialises
    /// concurrent writers attached to the same daemon, while the
    /// `TtyLock` keeps a second daemon from coming up at all.
    pub write_guard: Arc<tokio::sync::Mutex<()>>,
    /// Discovered path of the control-plane listener, used by callers
    /// (e.g. `bookrack exec`) that read the session lock and want to
    /// reach the control plane.
    pub control_sock: ControlSocketPath,
    /// Set by the platform signal aggregator before forwarding the
    /// broadcast; callers read it to decide whether to fast-path
    /// through `std::process::exit` instead of waiting for the
    /// foreground future to drain.
    pub signal_triggered: Arc<AtomicBool>,
    /// Notification handle the GUI tray (if any) waits on. The
    /// `tray.focus` control-plane method signals one waiter per call.
    /// Held here so a GUI entry that builds a `DaemonRuntime` in the
    /// same process can clone the handle and attach its own waiter.
    pub tray_focus_signal: Arc<tokio::sync::Notify>,
    /// Worker-loop pause flag. `queue.pause` flips it to `true`, the
    /// worker idles instead of pulling pending jobs; `queue.resume`
    /// flips it back. Mirrors `QueueState::paused` so the on-disk
    /// snapshot and the in-memory worker behaviour agree.
    pub queue_paused: Arc<AtomicBool>,
    /// Bundle of cheap shared handles the control-plane dispatcher
    /// runs against. A GUI host that builds the runtime in-process
    /// clones this to route webview and tray calls through
    /// `control::methods::dispatch` without a socket round-trip.
    pub method_context: MethodContext,
    /// Drop-only field: holds the session-scoped flock. The
    /// underscore prefix marks it as "kept alive for its destructor";
    /// no caller reads it.
    pub _tty_lock: TtyLock,
    queue_worker: Option<JoinHandle<Result<()>>>,
    signal_handle: JoinHandle<Result<()>>,
    control_accept_handle: JoinHandle<Result<()>>,
}

impl DaemonRuntime {
    /// Perform the eleven-step bring-up. Any step failing returns
    /// before the registry, queue worker, or signal task is left
    /// orphaned: the flock is released on drop, the broadcast hangs up
    /// on `shutdown_tx`'s last `Sender` going away, and any task
    /// already spawned receives the shutdown on its subscribed receiver.
    pub async fn start(opts: RuntimeOpts) -> Result<Self> {
        // 0. raise the soft RLIMIT_NOFILE before anything opens a
        //    descriptor; the outcome is logged once the tracing
        //    subscriber is installed in step 4.
        let nofile = crate::rlimit::raise_nofile();

        // 1. resolve runtime_dir; mkdir -p
        let runtime_dir = resolve_runtime_dir(opts.runtime_dir.as_deref())
            .context("resolve BOOKRACK_RUNTIME_DIR")?;
        std::fs::create_dir_all(&runtime_dir).with_context(|| {
            format!(
                "create runtime directory {} for the bookrack session lock",
                runtime_dir.display()
            )
        })?;

        // 2. resolve MCP listener address label up front so the lock
        //    file records it before the listener actually binds.
        let mcp_addr = if opts.no_mcp {
            None
        } else {
            Some(
                opts.mcp_addr
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| McpConfig::from_env().addr),
            )
        };
        let lock_path = runtime_dir.join(tty_lock_name());
        let mcp_label = mcp_addr.clone().unwrap_or_else(|| "disabled".to_string());

        // 3. TtyLock acquire (control_sock added in step 3b once the
        //    socket is bound; recording the path before the listener
        //    actually came up would let `bookrack exec` reach for a
        //    socket that never existed).
        let mut tty_lock = TtyLock::acquire(&lock_path, std::process::id(), &mcp_label, None)?;
        let started_at = Instant::now();
        let started_at_wall = Utc::now();
        tracing::info!(
            path = %lock_path.display(),
            mcp = %mcp_label,
            "bookrack session lock acquired",
        );

        // 3b. Bind the control-plane socket and record its path on
        //     the lock file. A bind failure releases the lock through
        //     the `TtyLock` drop and returns the error. The bound
        //     socket is held in a `ControlSocketGuard` so that any
        //     `?` between here and the final `Ok(Self)` unlinks the
        //     filesystem entry on the way out; once the runtime is
        //     fully stitched together the guard is `disarm`ed and
        //     ownership moves into `Self::control_sock`, after which
        //     cleanup happens in `run_until_shutdown`.
        let (control_listener, control_sock) =
            bind_control_socket(&runtime_dir).await.inspect_err(|_| {
                tracing::warn!("control socket bind failed; releasing session lock");
            })?;
        let control_sock_guard = ControlSocketGuard::new(control_sock);
        tty_lock
            .record_control_sock(control_sock_guard.path())
            .with_context(|| {
                format!(
                    "record control socket path {} in session lock",
                    control_sock_guard.path().display()
                )
            })?;
        tracing::info!(
            path = %control_sock_guard.path().display(),
            "control plane socket bound",
        );

        // 4. Config::resolve + obs init
        let cfg = Arc::new(Config::resolve(&opts.selection).context("resolve configuration")?);
        let (_obs_guard, log_stream) = bookrack_obs::init(&cfg, &opts.log_config);
        // The obs guard's lifetime ends with `DaemonRuntime`; leak the
        // guard so the subscriber stays installed until the process
        // exits. `bookrack_obs::init` returns a `WorkerGuard` whose
        // drop flushes the tracing subscriber's worker; leaking it is
        // the same shape `run_daemon` used historically — the runtime
        // owns no graceful shutdown for the writer thread.
        std::mem::forget(_obs_guard);
        match &nofile {
            Ok(None) => tracing::debug!("RLIMIT_NOFILE is unlimited"),
            Ok(Some(soft)) if *soft >= crate::rlimit::NOFILE_TARGET => {
                tracing::debug!(soft, "RLIMIT_NOFILE soft limit");
            }
            Ok(Some(soft)) => tracing::warn!(
                soft,
                target_limit = crate::rlimit::NOFILE_TARGET,
                "RLIMIT_NOFILE below target; a large ingest batch may exhaust file descriptors",
            ),
            Err(e) => tracing::warn!(error = %e, "failed to raise RLIMIT_NOFILE"),
        }

        // 5. EmbedConfig + OllamaEmbedClient
        let embed_cfg = EmbedConfig::from_env();
        let embedder = OllamaEmbedClient::new(
            cfg.ollama_url(),
            &embed_cfg.model,
            embed_cfg.request_timeout,
            embed_cfg.max_retries,
            embed_cfg.backoff_base,
        )
        .context("build embedding client")?;

        // 6. Catalog preflight: migrate each on-disk catalog forward
        //    to the binary's `TARGET_VERSION` (with the usual one-shot
        //    backup) before exposing a listener, then drop the handle
        //    so `Library::open` below claims the read-write connection
        //    fresh. A read-only check would refuse a database one
        //    schema behind the binary even though the migration step
        //    is forward-compatible — read-write open lets routine
        //    binary upgrades just work.
        if cfg.catalog_db().exists() {
            Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir())
                .context("preflight catalog schema check failed")?;
        }
        if cfg.papers_catalog_db().exists() {
            Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
                .context("preflight papers catalog schema check failed")?;
        }

        // 7. Library::open — books + papers. Each pipeline owns its
        //    chunk-version stamp; papers warms unconditionally so the
        //    first glean into an empty data dir lights up the read
        //    path the same way the book side does.
        let search_cfg = SearchConfig::from_env();
        let library = Library::open(
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            embedder,
            embed_cfg.model.clone(),
            search_cfg.top_k,
            bookrack_ingest::CHUNK_VERSION,
        )
        .await
        .context("open query library")?;
        let papers_embedder = OllamaEmbedClient::new(
            cfg.ollama_url(),
            &embed_cfg.model,
            embed_cfg.request_timeout,
            embed_cfg.max_retries,
            embed_cfg.backoff_base,
        )
        .context("build papers embedding client")?;
        let papers_library = Library::open(
            cfg.papers_corpus_db(),
            cfg.papers_catalog_db(),
            &cfg.papers_lancedb_dir(),
            papers_embedder,
            embed_cfg.model.clone(),
            search_cfg.top_k,
            bookrack_glean::CHUNK_VERSION,
        )
        .await
        .context("open papers query library")?
        .with_kind(bookrack_core::ItemKind::Paper);
        let papers_paths = PapersPaths {
            corpus_db: cfg.papers_corpus_db(),
            catalog_db: cfg.papers_catalog_db(),
            lancedb_dir: cfg.papers_lancedb_dir(),
            papers_dir: cfg.papers_dir(),
        };

        // 8. Ops::with_library; LibraryRegistry::single
        let ops = Ops::with_library(
            library,
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            cfg.books_dir(),
            cfg.backup_dir(),
            opts.caller,
        )
        .with_papers(papers_library, papers_paths);
        let library_name = cfg.library().unwrap_or("default").to_string();
        let handle = LibraryHandle::new(&library_name, ops);
        let registry = LibraryRegistry::single(handle);
        tracing::info!(library = %library_name, "library registry warmed up");

        // 9. LibraryInfoContext
        let info_context = LibraryInfoContext {
            data_dir: cfg.data_dir().display().to_string(),
            library_name: cfg.library().map(str::to_string),
            resolution_source: resolution_source_label(cfg.source()).to_string(),
            ollama_url: cfg.ollama_url().to_string(),
            embed_model_configured: embed_cfg.model.clone(),
            mcp_addr: mcp_label.clone(),
        };

        // 10. broadcast::channel; signal_task::spawn
        let (shutdown_tx, _) = broadcast::channel::<()>(8);
        let signal_triggered = Arc::new(AtomicBool::new(false));
        let signal_handle = tokio::spawn(signal_task(
            shutdown_tx.clone(),
            Arc::clone(&signal_triggered),
        ));

        // 11. queue state load + (opt) worker spawn
        let queue_state_path = cfg.data_dir().join(".bookrack-queue.json");
        let initial_queue_state =
            queue::load(&queue_state_path).context("load persistent queue state")?;
        let queue_paused = Arc::new(AtomicBool::new(initial_queue_state.paused));
        let queue_state = Arc::new(Mutex::new(initial_queue_state));
        let queue_params_template = build_queue_params_template(&cfg, &embed_cfg);
        let glean_params_template = build_glean_params_template(&cfg, &embed_cfg);

        // Event stream, daemon-state flag, and control accept loop
        // come up after the queue state is loaded so the initial
        // snapshot includes the on-disk queue.
        let state_flag = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let event_stream =
            EventStreamHandle::new(DEFAULT_EVENT_CHANNEL_CAPACITY, Arc::clone(&state_flag));
        let write_guard = Arc::new(tokio::sync::Mutex::new(()));
        let selection_for_doctor = LibrarySelection {
            data_dir: opts.selection.data_dir.clone(),
            library: opts.selection.library.clone(),
        };

        let queue_worker = if opts.spawn_queue_worker {
            let registry = Arc::clone(&registry);
            let state = Arc::clone(&queue_state);
            let state_path = queue_state_path.clone();
            let params_template = queue_params_template.clone();
            let glean_template = glean_params_template.clone();
            let shutdown_rx = shutdown_tx.subscribe();
            let library_default = library_name.clone();
            let events_for_loop = event_stream.clone();
            let events_for_runner = event_stream.clone();
            let queue_paused_worker = Arc::clone(&queue_paused);
            Some(tokio::spawn(queue::worker_loop(
                state_path,
                state,
                shutdown_rx,
                move |job| {
                    let registry = Arc::clone(&registry);
                    let params_template = params_template.clone();
                    let glean_template = glean_template.clone();
                    let library_default = library_default.clone();
                    let sink = EventProgressSink::new(job.id.clone(), events_for_runner.clone());
                    async move {
                        sink.report(Stage::Extract, None, None);
                        let outcome = tokio::task::spawn_blocking(move || {
                            let runtime = tokio::runtime::Handle::current();
                            let library = if job.library.is_empty() {
                                library_default
                            } else {
                                job.library.clone()
                            };
                            let force = job.force;
                            let hold_for_metadata = job.hold_for_metadata;
                            let job_kind = job.kind;
                            let path = job.path.clone();
                            let intake_ocr = job.intake_ocr.clone();
                            runtime.block_on(async move {
                                let handle = registry
                                    .get(Some(&library))
                                    .map_err(|e| queue::JobError::Book(format!("registry: {e}")))?;
                                if let Some(ocr) = intake_ocr {
                                    let mut params = params_template;
                                    params.force = force;
                                    params.hold_for_metadata = hold_for_metadata;
                                    let ocr_params = bookrack_ingest::ocr::OcrIngestParams {
                                        expected_pages: ocr.expected_pages,
                                        allow_partial: ocr.allow_partial,
                                    };
                                    handle
                                        .ingest_ocr(&path, &ocr.from_pdf, &ocr_params, &params)
                                        .await
                                        .map_err(|e| queue::classify_ingest_error(&e))?;
                                    return Ok::<(), queue::JobError>(());
                                }
                                match job_kind {
                                    bookrack_core::ItemKind::Book => {
                                        let mut params = params_template;
                                        params.force = force;
                                        params.hold_for_metadata = hold_for_metadata;
                                        handle
                                            .ingest_book(&path, &params)
                                            .await
                                            .map_err(|e| queue::classify_ingest_error(&e))?;
                                    }
                                    bookrack_core::ItemKind::Paper => {
                                        let mut params = glean_template;
                                        params.force = force;
                                        handle
                                            .glean_paper(&path, &params)
                                            .await
                                            .map_err(|e| queue::classify_ingest_error(&e))?;
                                    }
                                    bookrack_core::ItemKind::Reference => {
                                        // distill pipeline lands in a later route step;
                                        // until then a reference job in the queue is a
                                        // submission bug, not a runtime condition.
                                        unreachable!(
                                            "reference jobs are routed through the distill \
                                             worker (not wired yet); see route R5"
                                        );
                                    }
                                }
                                Ok::<(), queue::JobError>(())
                            })
                        })
                        .await
                        .map_err(|e| queue::JobError::Book(format!("queue worker join: {e}")))?;
                        if outcome.is_ok() {
                            sink.report(Stage::Embed, None, None);
                        }
                        outcome
                    }
                },
                events_for_loop,
                queue_paused_worker,
            )))
        } else {
            None
        };

        // Spawn the control-plane accept loop. The accept loop owns
        // the listener; per-connection tasks reuse the same broadcast
        // so a `shutdown_tx.send(())` tears down both the loop and
        // every attached client.
        let mcp_tools = Arc::new(opts.mcp_tools);
        let tray_focus_signal = Arc::new(tokio::sync::Notify::new());
        let plan_registry = Arc::new(crate::control::plan_registry::PlanRegistry::new());
        let method_ctx = MethodContext {
            cfg: Arc::clone(&cfg),
            registry: Arc::clone(&registry),
            info_context: info_context.clone(),
            queue_state: Arc::clone(&queue_state),
            queue_state_path: queue_state_path.clone(),
            event_stream: event_stream.clone(),
            write_guard: Arc::clone(&write_guard),
            shutdown_tx: shutdown_tx.clone(),
            started_at_rfc3339: started_at_wall.to_rfc3339(),
            selection: selection_for_doctor,
            library_name: library_name.clone(),
            mcp_tools,
            queue_worker_enabled: opts.spawn_queue_worker,
            tray_focus_signal: Arc::clone(&tray_focus_signal),
            queue_paused: Arc::clone(&queue_paused),
            log_stream: log_stream.clone(),
            plan_registry,
        };

        // Bridge the obs log stream into the control-plane event
        // stream so subscribers to the `log` channel receive every
        // tracing event the daemon emits. The bridge dies when the
        // log broadcast channel closes (i.e. when the obs guard
        // drops at runtime shutdown).
        let log_bridge_events = event_stream.clone();
        let mut log_rx = log_stream.subscribe();
        tokio::spawn(async move {
            loop {
                match log_rx.recv().await {
                    Ok(ev) => log_bridge_events.publish(Event::Log(ev)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let control_accept_handle = tokio::spawn(run_accept_loop(
            control_listener,
            method_ctx.clone(),
            shutdown_tx.subscribe(),
        ));

        // Every fallible step has cleared. Move the socket path out
        // of the guard so `Drop` no longer unlinks it; cleanup now
        // belongs to `run_until_shutdown`.
        let control_sock = control_sock_guard.disarm();

        Ok(Self {
            cfg,
            registry,
            info_context,
            log_stream,
            queue_state,
            queue_state_path,
            queue_params_template,
            shutdown_tx,
            runtime_dir,
            lock_path,
            started_at,
            started_at_wall,
            mcp_label,
            event_stream,
            write_guard,
            control_sock,
            signal_triggered,
            tray_focus_signal,
            queue_paused,
            method_context: method_ctx,
            _tty_lock: tty_lock,
            queue_worker,
            signal_handle,
            control_accept_handle,
        })
    }

    /// Block on the shared shutdown broadcast, then drain every
    /// spawned task with a fixed three-second timeout.
    ///
    /// `repl_handle` is required because the daemon always has a
    /// foreground task (the REPL on a TTY, an `std::thread::park` on
    /// stdin redirection). `mcp_handle` is optional because
    /// `--no-mcp` skips the listener entirely.
    pub async fn run_until_shutdown(
        self,
        mcp_handle: Option<JoinHandle<Result<()>>>,
        repl_handle: JoinHandle<Result<()>>,
    ) -> Result<()> {
        let Self {
            shutdown_tx,
            signal_triggered,
            queue_worker,
            signal_handle,
            control_accept_handle,
            control_sock,
            event_stream,
            // Bound (not folded into `..`) so the flock lives across
            // the drain timeouts below; `..` would drop it here.
            _tty_lock,
            ..
        } = self;

        let mut foreground_rx = shutdown_tx.subscribe();
        let _ = foreground_rx.recv().await;
        tracing::info!("shutdown signalled, joining session tasks");

        // Flip the daemon-state flag before draining clients so the
        // `daemon.state=stopping` notification reaches every attached
        // subscriber before its connection task exits.
        event_stream.set_state(DaemonState::Stopping);

        match tokio::time::timeout(Duration::from_secs(3), control_accept_handle).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => tracing::warn!(error = %err, "control accept loop returned error"),
            Ok(Err(err)) => tracing::warn!(error = %err, "control accept loop join failed"),
            Err(_) => tracing::warn!("control accept loop did not exit within 3s; abandoning"),
        }
        control_sock.cleanup();

        if let Some(handle) = mcp_handle {
            match tokio::time::timeout(Duration::from_secs(3), handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(err))) => tracing::warn!(error = %err, "MCP task returned error"),
                Ok(Err(err)) => tracing::warn!(error = %err, "MCP task join failed"),
                Err(_) => tracing::warn!("MCP task did not exit within 3s; abandoning"),
            }
        }

        if let Some(handle) = queue_worker {
            match tokio::time::timeout(Duration::from_secs(3), handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(err))) => tracing::warn!(error = %err, "queue worker returned error"),
                Ok(Err(err)) => tracing::warn!(error = %err, "queue worker join failed"),
                Err(_) => tracing::warn!("queue worker did not exit within 3s; abandoning"),
            }
        }

        signal_handle.abort();
        // REPL thread may still be blocked on `read_line` if shutdown
        // came from a signal; abort the join handle and let the OS
        // reap the blocking worker on process tear-down.
        repl_handle.abort();

        if signal_triggered.load(Ordering::SeqCst) {
            std::process::exit(0);
        }
        Ok(())
    }
}

/// Build the [`IngestParams`] template the queue worker reuses for
/// every job.
fn build_queue_params_template(cfg: &Config, embed_cfg: &EmbedConfig) -> IngestParams {
    IngestParams {
        embed: embed_cfg.clone(),
        hold_for_metadata: false,
        force: false,
        audit_data: load_audit_data(cfg),
        audit_profile: load_audit_profile(cfg, None),
        heading_patterns: load_heading_patterns(cfg),
        ..Default::default()
    }
}

/// Build the [`GleanParams`] template the queue worker reuses for
/// every paper-side job. Mirrors [`build_queue_params_template`] for
/// the book side: only `force` is patched per-job at dispatch time.
fn build_glean_params_template(cfg: &Config, embed_cfg: &EmbedConfig) -> GleanParams {
    GleanParams {
        embed: embed_cfg.clone(),
        extract_profile: load_audit_profile(cfg, None),
        heading_patterns: load_heading_patterns(cfg),
        paper_audit_profile: load_paper_audit_profile(cfg, None),
        paper_audit_data: load_paper_audit_data(cfg),
        ..Default::default()
    }
}

/// Map [`ResolutionSource`] onto the operator-visible label woven into
/// `session.info` and the daemon's startup banner.
pub fn resolution_source_label(source: ResolutionSource) -> &'static str {
    match source {
        ResolutionSource::DataDirFlag => "--data-dir flag",
        ResolutionSource::LibraryFlag => "--library flag",
        ResolutionSource::EnvVar => "BOOKRACK_DATA_DIR env",
        ResolutionSource::PortableExeNeighbor => "portable layout",
        ResolutionSource::RegistryDefault => "registry default",
        ResolutionSource::DefaultRegistryDefault => "default registry default",
        ResolutionSource::Explicit => "explicit",
    }
}

/// Aggregate the platform's shutdown signals onto the shared broadcast.
async fn signal_task(shutdown_tx: broadcast::Sender<()>, triggered: Arc<AtomicBool>) -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
        let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
        let mut sighup = signal(SignalKind::hangup()).context("install SIGHUP handler")?;
        tokio::select! {
            _ = sigint.recv() => tracing::info!("received SIGINT"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sighup.recv() => tracing::info!("received SIGHUP"),
        }
    }
    #[cfg(windows)]
    {
        let mut close =
            tokio::signal::windows::ctrl_close().context("install Ctrl-Close handler")?;
        tokio::select! {
            res = tokio::signal::ctrl_c() => {
                res.context("await Ctrl-C")?;
                tracing::info!("received Ctrl-C");
            }
            _ = close.recv() => tracing::info!("received Ctrl-Close"),
        }
    }
    triggered.store(true, Ordering::SeqCst);
    let _ = shutdown_tx.send(());
    Ok(())
}
