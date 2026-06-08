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
    pub mcp_label: String,
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

        // 3. TtyLock acquire
        let tty_lock = TtyLock::acquire(&lock_path, std::process::id(), &mcp_label)?;
        let started_at = Instant::now();
        tracing::info!(
            path = %lock_path.display(),
            mcp = %mcp_label,
            "bookrack session lock acquired",
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

        let queue_worker = if opts.spawn_queue_worker {
            let registry = Arc::clone(&registry);
            let state = Arc::clone(&queue_state);
            let state_path = queue_state_path.clone();
            let params_template = queue_params_template.clone();
            let shutdown_rx = shutdown_tx.subscribe();
            let library_default = library_name.clone();
            Some(tokio::spawn(queue::worker_loop(
                state_path,
                state,
                shutdown_rx,
                move |job| {
                    let registry = Arc::clone(&registry);
                    let params_template = params_template.clone();
                    let library_default = library_default.clone();
                    async move {
                        tokio::task::spawn_blocking(move || {
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
                        .map_err(|e| format!("queue worker join: {e}"))?
                    }
                },
            )))
        } else {
            None
        };

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
            mcp_label,
            signal_triggered,
            _tty_lock: tty_lock,
            queue_worker,
            signal_handle,
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
            ..
        } = self;

        let mut foreground_rx = shutdown_tx.subscribe();
        let _ = foreground_rx.recv().await;
        tracing::info!("shutdown signalled, joining session tasks");

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
