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
use bookrack_ingest::IngestParams;
use bookrack_obs::stream::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use bookrack_query::Library;
use bookrack_session::{TtyLock, resolve_runtime_dir, tty_lock_name};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::audit_helpers::{load_audit_data, load_audit_profile, load_heading_patterns};
use crate::control::events::{
    DEFAULT_EVENT_CHANNEL_CAPACITY, DaemonState, DaemonStateFlag, Event, EventStreamHandle, Stage,
};
use crate::control::methods::MethodContext;
use crate::control::progress::{EventProgressSink, ProgressSink};
use crate::control::socket::{ControlSocketPath, bind as bind_control_socket, run_accept_loop};
use crate::queue;

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
    /// [`Caller`] tag woven into [`Ops`] audit rows so the catalog
    /// distinguishes daemon-REPL writes from MCP-driven writes.
    pub caller: Caller,
    /// MCP tool surface, as published by the live `BookrackServer`.
    /// Empty for entry points that do not bring up the MCP listener;
    /// otherwise populated by the caller from
    /// `bookrack_mcp::list_tools()` so the control-plane
    /// `daemon.mcp_tools` method can answer without spinning up an
    /// MCP transport.
    pub mcp_tools: Vec<crate::control::methods::meta::McpToolInfo>,
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
            caller: Caller::mcp(),
            mcp_tools: Vec::new(),
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
    /// broadcast; daemon-REPL callers read it to decide whether to
    /// fast-path through `std::process::exit` (signal-driven shutdown
    /// leaves the synchronous reedline thread parked).
    pub signal_triggered: Arc<AtomicBool>,
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
        //     the `TtyLock` drop and returns the error.
        let (control_listener, control_sock) =
            bind_control_socket(&runtime_dir).await.inspect_err(|_| {
                tracing::warn!("control socket bind failed; releasing session lock");
            })?;
        tty_lock
            .record_control_sock(&control_sock.path)
            .with_context(|| {
                format!(
                    "record control socket path {} in session lock",
                    control_sock.path.display()
                )
            })?;
        tracing::info!(
            path = %control_sock.path.display(),
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

        // 6. Catalog preflight: reject a schema the binary cannot
        //    serve before exposing a listener.
        if cfg.catalog_db().exists() {
            Catalog::open_read_only(&cfg.catalog_db())
                .context("preflight catalog schema check failed")?;
        }

        // 7. Library::open
        let search_cfg = SearchConfig::from_env();
        let library = Library::open(
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            embedder,
            embed_cfg.model.clone(),
            search_cfg.top_k,
        )
        .await
        .context("open query library")?;

        // 8. Ops::with_library; LibraryRegistry::single
        let ops = Ops::with_library(
            library,
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            cfg.books_dir(),
            cfg.backup_dir(),
            opts.caller,
        );
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
        let queue_state = Arc::new(Mutex::new(
            queue::load(&queue_state_path).context("load persistent queue state")?,
        ));
        let queue_params_template = build_queue_params_template(&cfg, &embed_cfg);

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
            let shutdown_rx = shutdown_tx.subscribe();
            let library_default = library_name.clone();
            let events_for_loop = event_stream.clone();
            let events_for_runner = event_stream.clone();
            Some(tokio::spawn(queue::worker_loop(
                state_path,
                state,
                shutdown_rx,
                move |job| {
                    let registry = Arc::clone(&registry);
                    let params_template = params_template.clone();
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
                            let mut params = params_template;
                            params.force = job.force;
                            runtime.block_on(async move {
                                let handle = registry
                                    .get(Some(&library))
                                    .map_err(|e| format!("registry: {e}"))?;
                                handle
                                    .ingest_book(&job.path, &params)
                                    .await
                                    .map_err(|e| format!("ingest: {e:#}"))?;
                                Ok::<(), String>(())
                            })
                        })
                        .await
                        .map_err(|e| format!("queue worker join: {e}"))?;
                        if outcome.is_ok() {
                            sink.report(Stage::Embed, None, None);
                        }
                        outcome
                    }
                },
                events_for_loop,
            )))
        } else {
            None
        };

        // Spawn the control-plane accept loop. The accept loop owns
        // the listener; per-connection tasks reuse the same broadcast
        // so a `shutdown_tx.send(())` tears down both the loop and
        // every attached client.
        let mcp_tools = Arc::new(opts.mcp_tools);
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
            method_ctx,
            shutdown_tx.subscribe(),
        ));

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
