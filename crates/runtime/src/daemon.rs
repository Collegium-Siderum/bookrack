// SPDX-License-Identifier: Apache-2.0

//! Daemon-side process runtime behind `bookrack run`, and the
//! in-process hosts the tests build.
//!
//! [`DaemonRuntime::start`] performs the fixed eleven-step bring-up:
//! resolve the runtime directory, acquire the session [`TtyLock`],
//! decide the mount set and acquire each candidate root's
//! [`RootLock`], initialise observability, preflight each candidate's
//! embed backend and reranker stage, build one warm handle per
//! candidate ([`build_library_handle`]: embedding clients, catalog
//! preflight, query [`Library`] opens, the [`Ops`] wrap), assemble
//! the [`LibraryRegistry`] with the libraries that failed any of
//! those steps recorded beside it, install the platform signal
//! aggregator, load the persistent ingest queue state, and — in the
//! `bookrack run` profile — spawn the queue worker.
//!
//! Callers wire the MCP listener and any REPL surface as separate
//! [`tokio::task::JoinHandle`]s and hand them to
//! [`DaemonRuntime::run_until_shutdown`], which joins each one through
//! the shared `broadcast::Sender<()>`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, EmbedConfig, LibraryEntry, LibraryIdentification, LibrarySelection, LogConfig,
    McpConfig, ResolutionSource, RootIdentity, SearchConfig,
};
use bookrack_core::Problem;
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_glean::GleanParams;
use bookrack_ingest::IngestParams;
use bookrack_obs::WorkerGuard;
use bookrack_obs::stream::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{
    JobTemplates, LibraryHandle, LibraryRegistry, MountGuard, UnmountedLibrary,
};
use bookrack_ops::{Caller, Ops, PapersPaths};
use bookrack_query::Library;
use bookrack_session::{
    RootLock, TtyLock, is_root_lock_conflict, resolve_runtime_dir, root_lock_path, tty_lock_name,
};
use eyre::{Context, Result};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::audit_helpers::{
    load_audit_data, load_audit_profile, load_heading_patterns, load_paper_audit_data,
    load_paper_audit_profile,
};
use crate::control::events::{
    DEFAULT_EVENT_CHANNEL_CAPACITY, DaemonState, DaemonStateFlag, DegradedCause, Event,
    EventStreamHandle, Stage,
};
use crate::control::methods::MethodContext;
use crate::control::progress::{
    EMBED_PROGRESS_INTERVAL, EventProgressSink, ProgressSink, embed_batch_reporter,
};
use crate::control::socket::{
    ControlSocketGuard, ControlSocketPath, bind as bind_control_socket, run_accept_loop,
};
use crate::queue;
use crate::rerank_supervisor::SupervisorState;
use crate::unmounted::{self, MountOccasion, Skips};

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
    /// Spawn the persistent ingest queue worker. `false` leaves the
    /// queue state inert, exposed as zero counts; the [`headless`]
    /// preset and in-process hosts use it.
    ///
    /// [`headless`]: RuntimeOpts::headless
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
}

impl RuntimeOpts {
    /// Preset with no REPL and no queue worker: env-derived MCP
    /// address, console layer mirroring the file directive. For
    /// in-process hosts and tests.
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
        }
    }
}

/// Shared daemon state. `bookrack run` builds one and hands it to the
/// MCP listener, the REPL, and the queue worker
/// as a single source of truth for the open library registry, the
/// broadcast shutdown channel, and the on-disk queue state.
pub struct DaemonRuntime {
    pub cfg: Arc<Config>,
    pub registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    pub info_context: LibraryInfoContext,
    pub log_stream: LogStreamHandle,
    pub queue_state: Arc<Mutex<QueueState>>,
    pub queue_state_path: PathBuf,
    pub shutdown_tx: broadcast::Sender<()>,
    pub runtime_dir: PathBuf,
    pub lock_path: PathBuf,
    pub started_at: Instant,
    /// Wall-clock instant the daemon entered service. Reported to
    /// control-plane clients through `daemon.version` so they can
    /// derive an uptime.
    pub started_at_wall: DateTime<Utc>,
    /// Address the MCP listener bound, or `disabled` when the session
    /// runs without one. Set from the socket's own `local_addr`, so a
    /// `port 0` configuration reports the port the kernel assigned.
    pub mcp_label: String,
    /// The bound MCP socket, taken by the host that spawns the
    /// listener task ([`bookrack_mcp::spawn_listener`]). `None` once
    /// taken, and from the start when the session runs without MCP.
    pub mcp_listener: Option<tokio::net::TcpListener>,
    /// Broadcast handle for control-plane events.
    pub event_stream: EventStreamHandle,
    /// Process-wide write mutex held by every control-plane write
    /// handler for the duration of its underlying business call.
    /// Distinct from the cross-process [`TtyLock`]: this serialises
    /// concurrent writers attached to the same daemon, while the
    /// `TtyLock` keeps a second daemon from coming up at all.
    pub write_guard: Arc<tokio::sync::Mutex<()>>,
    /// Discovered path of the control-plane listener, used by callers
    /// (e.g. `bookrack rpc`) that read the session lock and want to
    /// reach the control plane.
    pub control_sock: ControlSocketPath,
    /// Set by the platform signal aggregator before forwarding the
    /// broadcast; callers read it to decide whether to fast-path
    /// through `std::process::exit` instead of waiting for the
    /// foreground future to drain.
    pub signal_triggered: Arc<AtomicBool>,
    /// Worker-loop pause flag. `queue.pause` flips it to `true`, the
    /// worker idles instead of pulling pending jobs; `queue.resume`
    /// flips it back. Mirrors `QueueState::paused` so the on-disk
    /// snapshot and the in-memory worker behaviour agree.
    pub queue_paused: Arc<AtomicBool>,
    /// Bundle of cheap shared handles the control-plane dispatcher
    /// runs against. An in-process host clones this to route calls
    /// through `control::methods::dispatch` without a socket
    /// round-trip.
    pub method_context: MethodContext,
    /// The supervised llama-server behind the reranker stage, when
    /// the effective profile enables one and no operator URL takes
    /// over. Stopped (TERM, grace, KILL) during shutdown.
    pub rerank_supervisor: Option<Arc<crate::rerank_supervisor::RerankSupervisor>>,
    /// Drop-only field: holds the session-scoped flock. The
    /// underscore prefix marks it as "kept alive for its destructor";
    /// no caller reads it.
    pub _tty_lock: TtyLock,
    /// Drop-only field: takes every library out of the registry, which
    /// releases each served root's lock as soon as the last caller
    /// still using that library is done. Each lock lives on its
    /// library's handle, so this is the one owner whose drop stands for
    /// "this daemon is done serving" — a spawned task holding a
    /// `MethodContext` clone keeps the registry alive on its own.
    _mounted_set: crate::mount::MountedSet,
    /// Drop-only field: holds the tracing non-blocking writer's
    /// background-thread guard. Dropping flushes buffered log lines
    /// and joins the writer thread, so this lives until the runtime
    /// is torn down (either because bring-up after `obs::init` failed
    /// or the daemon exited cleanly).
    _obs_guard: Option<WorkerGuard>,
    /// Receiver subscribed when the shutdown channel is created,
    /// consumed by [`DaemonRuntime::run_until_shutdown`]'s foreground
    /// wait. Held here — rather than subscribed inside the drain — so
    /// a shutdown fired between bring-up and the drain (a signal, an
    /// early `daemon.shutdown`, or an embedding caller's direct
    /// `shutdown_tx.send`) is buffered for the drain instead of lost:
    /// a broadcast subscriber only sees messages sent after it
    /// subscribes.
    shutdown_rx: broadcast::Receiver<()>,
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

        // 2. resolve the MCP listener address up front so the lock
        //    file records a complete session record from the instant
        //    it exists. This is the *intended* address; step 3c
        //    replaces it with the one the listener actually bound.
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
        let mcp_intent = mcp_addr.clone().unwrap_or_else(|| "disabled".to_string());

        // 3. TtyLock acquire (control_sock added in step 3b once the
        //    socket is bound; recording the path before the listener
        //    actually came up would let `bookrack rpc` reach for a
        //    socket that never existed).
        let mut tty_lock = TtyLock::acquire(&lock_path, std::process::id(), &mcp_intent, None)?;
        let started_at = Instant::now();
        let started_at_wall = Utc::now();
        tracing::info!(
            path = %lock_path.display(),
            mcp = %mcp_intent,
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

        // 3c. Bind the MCP listener, and refuse the bring-up if the
        //     endpoint cannot be taken. Deliberately after the session
        //     lock: a second daemon on the same runtime directory must
        //     hear "already running", not "port in use". The bound
        //     socket travels to the listener task through
        //     `Self::mcp_listener`, so the address reported from here
        //     on is one this process owns — with `port 0` it is the
        //     port the kernel assigned, which no earlier step knows.
        let mcp_listener = match &mcp_addr {
            Some(addr) => Some(crate::mcp_endpoint::bind_listener(addr).await?),
            None => None,
        };
        let mcp_label = match &mcp_listener {
            Some(listener) => listener
                .local_addr()
                .context("read the bound MCP listener address")?
                .to_string(),
            None => "disabled".to_string(),
        };
        if mcp_listener.is_some() {
            tty_lock.record_mcp_addr(&mcp_label)?;
            tracing::info!(mcp = %mcp_label, "MCP listener bound");
        }

        // 4. Config::resolve + obs init
        //
        // The authoritative resolve, and the only one whose result is
        // used. `bookrack run` probes the same call before taking the
        // lock, to offer the wizard on an unconfigured install, and
        // throws that result away; resolution happens here, after the
        // lock, so the configuration the daemon serves is the one that
        // was in place when it took ownership.
        let mut cfg = Config::resolve(&opts.selection).context("resolve configuration")?;
        // 4a. A path-class root whose manifest identity the registry
        //     records at another path is refused before anything is
        //     locked or opened: serving it under the registered name
        //     would redirect the daemon to the recorded root, and
        //     serving it anonymously would run one identity in two
        //     places.
        refuse_relocated_identity(&cfg)?;
        // 4a'. A path-class root with a manifest the registry does not
        //      know is registered under the manifest's name, then the
        //      selection is resolved again so the name is claimed the
        //      way any registered root's is and every consumer below
        //      reads it off `cfg`.
        let auto_registered: Vec<String> = match crate::auto_register::auto_register(&cfg) {
            crate::auto_register::AutoRegistration::Registered { name } => {
                tracing::info!(
                    library = %name,
                    root = %cfg.data_dir().display(),
                    "registered the selected root under its manifest name",
                );
                cfg = Config::resolve(&opts.selection)
                    .context("resolve configuration after registering the selected root")?;
                vec![name]
            }
            crate::auto_register::AutoRegistration::Skipped(skip) => {
                skip.log(cfg.data_dir());
                Vec::new()
            }
        };
        let cfg = Arc::new(cfg);
        // 4b. Decide the mount set. With the primary root selected
        //     through the registry the daemon serves every registered
        //     library; a path-class root the registry does not know (or
        //     a machine with no registry) keeps the single-library form.
        //     From here to step 8 each candidate stands or falls on its
        //     own: a library that fails a step is skipped with its
        //     reason, its lock is released with it, and the rest go on.
        //     The set changes at runtime through `library.mount` /
        //     `library.unmount`.
        let primary_name = cfg.library().unwrap_or("default").to_string();
        let (registry_entries, registry_default) = if cfg.library().is_some() {
            bookrack_config::list_libraries_with_default()
                .context("read the library registry for eager mounting")?
                .unwrap_or_default()
        } else {
            (Vec::new(), None)
        };
        let plan = plan_mounts(
            &primary_name,
            &registry_entries,
            registry_default.as_deref(),
        );
        let mut skips = Skips::default();
        let mut candidates: Vec<Candidate> = Vec::with_capacity(plan.names.len());
        if plan.eager {
            for name in &plan.names {
                let entry = registry_entries.iter().find(|entry| &entry.name == name);
                let data_dir = entry
                    .map(|entry| entry.data_dir.clone())
                    .unwrap_or_default();
                // A named selection resolves through the registry and
                // is immune to a set BOOKRACK_DATA_DIR.
                let lib_cfg = match Config::resolve(&LibrarySelection {
                    data_dir: None,
                    library: Some(name.clone()),
                }) {
                    Ok(lib_cfg) => lib_cfg,
                    Err(err) => {
                        skips.push_worded(
                            UnmountedLibrary::new(
                                name,
                                data_dir,
                                unmounted::resolve_problem(name, &err, MountOccasion::Serving),
                            ),
                            unmounted::resolve_problem(name, &err, MountOccasion::Refusing),
                            eyre::Report::new(err)
                                .wrap_err(format!("resolve configuration for library '{name}'")),
                        );
                        continue;
                    }
                };
                // A root that does not carry the identity the entry
                // cached is skipped before it is locked: an empty mount
                // point passes the directory check and would otherwise
                // be served as an empty library. The record is worded
                // for the daemon that goes on serving; the refusal, for
                // a bring-up left with nothing to serve.
                if let Some(entry) = entry
                    && let Err(fault) =
                        crate::identity_check::verify_registered_identity(entry, lib_cfg.data_dir())
                {
                    skips.push_worded(
                        UnmountedLibrary::new(
                            name,
                            lib_cfg.data_dir(),
                            fault.problem(MountOccasion::Serving),
                        ),
                        fault.problem(MountOccasion::Refusing),
                        fault.refusal(MountOccasion::Refusing).into(),
                    );
                    continue;
                }
                candidates.push(Candidate::new(name, Arc::new(lib_cfg)));
            }
        } else {
            candidates.push(Candidate::new(&primary_name, Arc::clone(&cfg)));
        }

        // 4c. One library per root. The default claims its root first,
        //     then the rest in registry order; a later claimant on a
        //     claimed root is skipped with a reason naming both.
        let mut roots_seen: HashMap<PathBuf, String> = HashMap::new();
        let mut claim_order: Vec<usize> = (0..candidates.len()).collect();
        if let Some(default) = plan.default_name.as_deref()
            && let Some(position) = candidates
                .iter()
                .position(|candidate| candidate.name == default)
        {
            claim_order.remove(position);
            claim_order.insert(0, position);
        }
        let mut duplicates: Vec<usize> = Vec::new();
        for index in claim_order {
            let candidate = &candidates[index];
            if let Some(previous) =
                claim_unique_root(&mut roots_seen, candidate.cfg.data_dir(), &candidate.name)
            {
                let root = candidate.cfg.data_dir();
                skips.push(
                    UnmountedLibrary::new(
                        &candidate.name,
                        root,
                        crate::mount::duplicate_root_problem(&previous, root),
                    ),
                    eyre::eyre!(
                        "registry entries '{previous}' and '{}' point at the same data root {}; \
                         fix the registry before starting the daemon",
                        candidate.name,
                        root.display(),
                    ),
                );
                duplicates.push(index);
            }
        }
        duplicates.sort_unstable();
        for index in duplicates.into_iter().rev() {
            candidates.remove(index);
        }

        // 4d. Take every candidate's root lock before anything expensive
        //     comes up, so a contended root is skipped with no reranker
        //     spawned and no half-open handle. A root that cannot host a
        //     lock file at all — a read-only volume, which `libraries
        //     add` already supports — is served unlocked: read-only
        //     media have no writers to exclude. The lock rides on the
        //     candidate, and from step 6 on the library's handle, so it
        //     is released when the holder is, not when this scope ends.
        let mut unlocked: Vec<(String, PathBuf, String)> = Vec::new();
        let mut kept = Vec::with_capacity(candidates.len());
        for mut candidate in candidates {
            match RootLock::acquire(candidate.cfg.data_dir(), std::process::id(), "daemon") {
                Ok(lock) => {
                    candidate.guard = Some(Arc::new(lock) as MountGuard);
                    kept.push(candidate);
                }
                Err(err) if is_root_lock_conflict(&err) => {
                    let problem = crate::mount::root_locked_problem(&err);
                    skips.push(
                        UnmountedLibrary::new(&candidate.name, candidate.cfg.data_dir(), problem),
                        err.wrap_err(format!(
                            "lock the data root of library '{}'",
                            candidate.name
                        )),
                    );
                }
                Err(err) => {
                    unlocked.push((
                        candidate.name.clone(),
                        candidate.cfg.data_dir().to_path_buf(),
                        format!("{err}"),
                    ));
                    kept.push(candidate);
                }
            }
        }
        let candidates = kept;
        // The daemon state directory holds process-scoped assets that
        // span libraries: the log stream (below) and the queue
        // snapshot (step 11). Resolved once, before the subscriber
        // needs its `logs/` subdirectory.
        let daemon_state_dir =
            bookrack_config::daemon_state_dir().context("resolve the daemon state directory")?;
        std::fs::create_dir_all(&daemon_state_dir).with_context(|| {
            format!(
                "create daemon state directory {}",
                daemon_state_dir.display()
            )
        })?;
        let (obs_guard, log_stream) =
            bookrack_obs::init(&daemon_state_dir.join("logs"), &opts.log_config);
        // The guard owns the non-blocking writer's background thread;
        // drop flushes buffered lines and joins the thread. Bind it
        // to `DaemonRuntime` so a `?` between here and the final
        // `Ok(Self)` runs the destructor cleanly instead of stranding
        // a writer thread on every failed bring-up.
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
        // The lock stage ran before the subscriber existed; its outcomes
        // are recorded now so the log carries them.
        for candidate in &candidates {
            if candidate.guard.is_some() {
                tracing::info!(
                    library = %candidate.name,
                    path = %root_lock_path(candidate.cfg.data_dir()).display(),
                    "bookrack data root lock acquired",
                );
            }
        }
        for (name, root, error) in &unlocked {
            tracing::warn!(
                error = %error,
                library = %name,
                root = %root.display(),
                "data root lock could not be created; serving the root unlocked",
            );
        }

        // 5. The primary's embed model, for the status card's snapshot
        //    alone: each handle resolves its own configuration in step
        //    6, and a primary whose profile does not resolve is skipped
        //    in 5a like any other candidate rather than refused here.
        let embed_model = crate::profile::effective_embed_config(&cfg)
            .map(|embed| embed.model)
            .unwrap_or_default();

        // 5a. Every candidate's embed configuration and backend, before
        //     anything expensive starts. Step 6 opens each library, and
        //     opening one probes `/api/embed` unconditionally — so a
        //     backend that cannot serve would fail the library anyway,
        //     just later and with the failure buried under a wrapper
        //     chain. Judging here costs nothing extra and puts one
        //     readable sentence and one command on the record; every
        //     candidate sharing an unusable backend is skipped, not
        //     only the first.
        //
        //     Ahead of 5b on purpose: `bring_up_reranker` may spawn a
        //     supervised llama-server and hold it to readiness, which
        //     is the most expensive step in the sequence. Paying for
        //     it before a bring-up that is certain to fail is waste
        //     the operator waits through.
        let mut kept = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            match crate::profile::effective_embed_config(&candidate.cfg) {
                Ok(_) => kept.push(candidate),
                Err(err) => {
                    let err = err.wrap_err(format!(
                        "resolve effective embed configuration for library '{}'",
                        candidate.name
                    ));
                    skips.push(
                        UnmountedLibrary::new(
                            &candidate.name,
                            candidate.cfg.data_dir(),
                            crate::control::error_map::report_problem(&err),
                        ),
                        err,
                    );
                }
            }
        }
        let mounts: Vec<(String, Arc<Config>)> = kept
            .iter()
            .map(|candidate| (candidate.name.clone(), Arc::clone(&candidate.cfg)))
            .collect();
        let backend_problems = crate::backend_probe::embed_backend_problems(&mounts).await;
        let mut candidates = Vec::with_capacity(kept.len());
        for candidate in kept {
            match backend_problems
                .iter()
                .find(|(name, _)| *name == candidate.name)
            {
                None => candidates.push(candidate),
                Some((_, problem)) => skips.push(
                    UnmountedLibrary::new(
                        &candidate.name,
                        candidate.cfg.data_dir(),
                        problem.clone(),
                    ),
                    crate::backend_probe::PreflightRefusal {
                        library: candidate.name.clone(),
                        problem: problem.clone(),
                    }
                    .into(),
                ),
            }
        }
        // Nothing left to serve, and nothing expensive paid for yet:
        // the one cause when there was one library, every library's
        // reason when there were several. Zero-library bring-up is a
        // later step.
        if candidates.is_empty() {
            return Err(skips.into_refusal());
        }

        // Event stream and daemon-state flag come up before the
        // reranker so the supervisor's state observer can drive the
        // degraded source from bring-up onward. The broadcast has no
        // subscribers yet; `events.subscribe` snapshots are built per
        // connection, after everything below is loaded.
        let state_flag = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let event_stream =
            EventStreamHandle::new(DEFAULT_EVENT_CHANNEL_CAPACITY, Arc::clone(&state_flag));

        // 5b. Reranker backend, before any listener serves: an
        //     operator URL is probed once, or a supervised
        //     llama-server is spawned and held to readiness. Either
        //     way a profile that promises a reranker gets a verified
        //     backend or the daemon refuses to start.
        //
        //     One backend serves every mounted library, so the set has
        //     to agree on the stage. The reference is the registry
        //     default when it is still a candidate, else the first
        //     candidate; a candidate whose profile disagrees with the
        //     reference is skipped rather than served under a stage it
        //     did not declare.
        let reference = plan
            .default_name
            .as_deref()
            .and_then(|default| {
                candidates
                    .iter()
                    .position(|candidate| candidate.name == default)
            })
            .unwrap_or(0);
        let reference_name = candidates[reference].name.clone();
        let reference_cfg = Arc::clone(&candidates[reference].cfg);
        let reference_spec = crate::rerank_supervisor::reranker_spec(&reference_cfg)
            .with_context(|| format!("resolve the index profile of library '{reference_name}'"))?;
        let mut kept = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if candidate.name == reference_name {
                kept.push(candidate);
                continue;
            }
            match crate::rerank_supervisor::reranker_spec(&candidate.cfg) {
                Ok(spec) if spec == reference_spec => kept.push(candidate),
                Ok(spec) => {
                    let problem = crate::rerank_supervisor::disagreement_problem(
                        &reference_name,
                        &reference_spec,
                        &candidate.name,
                        &spec,
                    );
                    skips.push(
                        UnmountedLibrary::new(
                            &candidate.name,
                            candidate.cfg.data_dir(),
                            problem.clone(),
                        ),
                        crate::backend_probe::PreflightRefusal {
                            library: candidate.name.clone(),
                            problem,
                        }
                        .into(),
                    );
                }
                Err(err) => {
                    let err = err.wrap_err(format!(
                        "resolve the index profile of library '{}'",
                        candidate.name
                    ));
                    skips.push(
                        UnmountedLibrary::new(
                            &candidate.name,
                            candidate.cfg.data_dir(),
                            crate::control::error_map::report_problem(&err),
                        ),
                        err,
                    );
                }
            }
        }
        let candidates = kept;
        let events_for_rerank = event_stream.clone();
        let reranker = crate::rerank_supervisor::bring_up_reranker(
            &reference_cfg,
            &runtime_dir,
            Some(Arc::new(move |state: &SupervisorState| {
                if let Some(active) = rerank_degraded_transition(state) {
                    events_for_rerank.set_degraded(DegradedCause::RerankCrashloop, active);
                }
            })),
        )
        .await
        .context("bring up the reranker backend")?;
        let rerank_supervisor = reranker.as_ref().and_then(|r| r.supervisor.clone());

        // 6.-8. One warm handle per candidate, then the registry. A
        //    library that fails to come up is skipped with its reason;
        //    its root lock went in with the call and is released with
        //    it. The skipped libraries are recorded beside the served
        //    ones, so a call naming one is refused with the reason.
        let mut handles = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let Candidate {
                name,
                cfg: lib_cfg,
                guard,
            } = candidate;
            match build_library_handle(
                Arc::clone(&lib_cfg),
                &name,
                reranker.as_ref().map(|r| &r.stage),
                opts.caller.clone(),
                guard,
            )
            .await
            {
                Ok(handle) => handles.push(handle),
                Err(err) => {
                    let err = err.wrap_err(format!("bring up library '{name}'"));
                    skips.push(
                        UnmountedLibrary::new(
                            &name,
                            lib_cfg.data_dir(),
                            crate::control::error_map::report_problem(&err),
                        ),
                        err,
                    );
                }
            }
        }
        if handles.is_empty() {
            return Err(skips.into_refusal());
        }
        let served = handles.len();
        let registry = LibraryRegistry::from_handles(handles, plan.default_name.clone())
            .context("assemble the library registry")?;
        for skip in skips.iter() {
            registry
                .record_unmounted(skip.library.clone())
                .context("record a library bring-up did not mount")?;
            tracing::warn!(
                library = %skip.library.name(),
                root = %skip.library.data_dir().display(),
                reason = %skip.library.reason().summary,
                hint = skip.library.reason().data.hint.as_deref().unwrap_or(""),
                "library not mounted at bring-up",
            );
        }
        // Single-value snapshot consumed by the `status` card and the
        // `library.changed` snapshot; it is not a routing fallback.
        // Per-library status surfaces are a later milestone.
        let library_name = primary_name.clone();
        tracing::info!(
            libraries = served,
            unmounted = skips.iter().count(),
            default = ?plan.default_name,
            auto_registered = ?auto_registered,
            "library registry warmed up",
        );

        // 9. LibraryInfoContext
        let info_context = library_info_context(&cfg, &embed_model, &mcp_label);

        // 10. broadcast::channel; signal_task::spawn
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(8);
        let signal_triggered = Arc::new(AtomicBool::new(false));
        let signal_handle = tokio::spawn(signal_task(
            shutdown_tx.clone(),
            Arc::clone(&signal_triggered),
            event_stream.clone(),
        ));

        // 11. queue state load + (opt) worker spawn. The snapshot lives
        //     in the daemon state directory, not under any library's
        //     data root: the queue spans libraries (each job carries its
        //     target `library`), so its document belongs to the daemon
        //     process. A snapshot written by a binary that kept it
        //     under the data root is moved over once.
        let queue_state_path = daemon_state_dir.join("queue.json");
        migrate_queue_snapshot(
            &cfg.data_dir().join(".bookrack-queue.json"),
            &queue_state_path,
        )
        .context("migrate the queue snapshot into the daemon state directory")?;
        // No eyre context: `QueueLoadError` is self-sufficient in the
        // log and carries its own three-part wording to the front end.
        let mut initial_queue_state = queue::load(&queue_state_path)?;
        // 11a. A row written before `library` was filled is pinned to
        //      the library the old unnamed route sent it to — the
        //      registry's default entry, else the bring-up library —
        //      and a row against a library bring-up skipped is failed
        //      with the reason. Both are redone at every start, so a
        //      save that fails is logged and the daemon comes up on the
        //      adjusted in-memory state.
        let pin_target = registry_entries
            .iter()
            .find(|entry| entry.is_default)
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| primary_name.clone());
        let mut adjusted = queue::pin_unnamed_jobs(&mut initial_queue_state, &pin_target);
        for library in skips.libraries() {
            let failure = queue::JobFailure {
                message: unmounted::queue_failure_message(library),
                problem: Some(library.reason().clone()),
            };
            adjusted +=
                queue::fail_jobs_against(&mut initial_queue_state, library.name(), &failure);
        }
        if adjusted > 0
            && let Err(err) = queue::save_atomic(&mut initial_queue_state, &queue_state_path)
        {
            tracing::error!(
                error = %err,
                path = %queue_state_path.display(),
                adjusted,
                "queue document could not be saved after bring-up adjusted its rows",
            );
        }
        let queue_paused = Arc::new(AtomicBool::new(initial_queue_state.paused));
        let queue_state = Arc::new(Mutex::new(initial_queue_state));
        let write_guard = Arc::new(tokio::sync::Mutex::new(()));
        let selection_for_doctor = LibrarySelection {
            data_dir: opts.selection.data_dir.clone(),
            library: opts.selection.library.clone(),
        };

        let queue_worker = if opts.spawn_queue_worker {
            let registry = Arc::clone(&registry);
            let state = Arc::clone(&queue_state);
            let state_path = queue_state_path.clone();
            let shutdown_rx = shutdown_tx.subscribe();
            let events_for_loop = event_stream.clone();
            let events_for_runner = event_stream.clone();
            let queue_paused_worker = Arc::clone(&queue_paused);
            Some(tokio::spawn(queue::worker_loop(
                state_path,
                state,
                shutdown_rx,
                move |job| {
                    let registry = Arc::clone(&registry);
                    let sink =
                        EventProgressSink::new(job.id.clone(), job.kind, events_for_runner.clone());
                    async move {
                        sink.report(Stage::Processing, None, None);
                        tokio::task::spawn_blocking(move || {
                            let runtime = tokio::runtime::Handle::current();
                            // A submit pins the job to a registry name
                            // at enqueue time, and bring-up pins every
                            // row that predates the field, so a row
                            // reaching here unnamed cannot be routed.
                            let library = job.library.clone();
                            let force = job.force;
                            let hold_for_metadata = job.hold_for_metadata;
                            let job_kind = job.kind;
                            let path = job.path.clone();
                            let intake_ocr = job.intake_ocr.clone();
                            let audit_profile = job.audit_profile.clone();
                            runtime.block_on(async move {
                                if library.is_empty() {
                                    return Err(queue::JobError::Book(queue::JobFailure::text(
                                        "this job names no library, so it cannot be routed; \
                                         retry it after a restart, which pins such rows",
                                    )));
                                }
                                let handle = registry.get(Some(&library)).map_err(|e| {
                                    queue::JobError::Book(queue::JobFailure::text(format!(
                                        "registry: {e}"
                                    )))
                                })?;
                                // Both book-pipeline legs below report
                                // their embedding pass through one
                                // throttled reporter on the job's sink.
                                let on_embed_batch =
                                    embed_batch_reporter(&sink, EMBED_PROGRESS_INTERVAL);
                                if let Some(ocr) = intake_ocr {
                                    let params = prepare_book_params(
                                        &handle.templates().ingest,
                                        handle.cfg(),
                                        force,
                                        hold_for_metadata,
                                        audit_profile.as_deref(),
                                    );
                                    let ocr_params = bookrack_ingest::ocr::OcrIngestParams {
                                        expected_pages: ocr.expected_pages,
                                        allow_partial: ocr.allow_partial,
                                    };
                                    let report = handle
                                        .ingest_ocr(
                                            &path,
                                            &ocr.from_pdf,
                                            &ocr_params,
                                            &params,
                                            &on_embed_batch,
                                        )
                                        .await
                                        .map_err(|e| queue::classify_ingest_error(&e))?;
                                    return Ok::<queue::JobSuccess, queue::JobError>(
                                        queue::JobSuccess {
                                            no_op: report.no_op,
                                            needs_ocr: false,
                                            intake_id: Some(report.ocr_intake_id),
                                        },
                                    );
                                }
                                match job_kind {
                                    bookrack_core::ItemKind::Book => {
                                        let params = prepare_book_params(
                                            &handle.templates().ingest,
                                            handle.cfg(),
                                            force,
                                            hold_for_metadata,
                                            audit_profile.as_deref(),
                                        );
                                        // A source with no usable text layer
                                        // is not a failure: ingest registered
                                        // a `needs_ocr` anchor before erroring,
                                        // so surface it as a non-failure
                                        // terminal carrying that anchor id.
                                        // Recognize it before `classify_ingest_error`
                                        // erases the typed error into a message.
                                        let report = match handle
                                            .ingest_book(&path, &params, &on_embed_batch)
                                            .await
                                        {
                                            Ok(report) => report,
                                            Err(e) => {
                                                if let Some(intake_id) = queue::ingest_needs_ocr(&e)
                                                {
                                                    return Ok::<queue::JobSuccess, queue::JobError>(
                                                        queue::JobSuccess {
                                                            no_op: false,
                                                            needs_ocr: true,
                                                            intake_id: Some(intake_id),
                                                        },
                                                    );
                                                }
                                                return Err(queue::classify_ingest_error(&e));
                                            }
                                        };
                                        Ok::<queue::JobSuccess, queue::JobError>(
                                            queue::JobSuccess {
                                                no_op: report.no_op,
                                                needs_ocr: false,
                                                intake_id: Some(report.intake_id),
                                            },
                                        )
                                    }
                                    bookrack_core::ItemKind::Paper => {
                                        // Paper jobs read their audit
                                        // profile from `glean_template`'s
                                        // paper-side field. The book-side
                                        // per-job override is enqueued as
                                        // `None` for paper kinds and is
                                        // intentionally not consulted here.
                                        let mut params = handle.templates().glean.clone();
                                        params.force = force;
                                        let report = handle
                                            .glean_paper(&path, &params)
                                            .await
                                            .map_err(|e| queue::classify_ingest_error(&e))?;
                                        Ok::<queue::JobSuccess, queue::JobError>(
                                            queue::JobSuccess {
                                                no_op: report.no_op,
                                                needs_ocr: false,
                                                intake_id: Some(report.intake_id),
                                            },
                                        )
                                    }
                                    bookrack_core::ItemKind::Reference => {
                                        // The distill pipeline is not yet routed
                                        // through the queue worker. A reference
                                        // job that reaches this point is a
                                        // submission-side bug; record it as a
                                        // per-item failure so the worker keeps
                                        // pulling subsequent jobs instead of
                                        // panicking and surfacing the failure
                                        // as a book-side backtrace.
                                        Err(queue::JobError::Book(queue::JobFailure::text(
                                            format!(
                                                "reference job {} cannot be processed: \
                                             the distill pipeline is not yet wired \
                                             through the queue worker",
                                                path.display()
                                            ),
                                        )))
                                    }
                                }
                            })
                        })
                        .await
                        .map_err(|e| {
                            queue::JobError::Book(queue::JobFailure::text(format!(
                                "queue worker join: {e}"
                            )))
                        })?
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
        let plan_registry = Arc::new(crate::control::plan_registry::PlanRegistry::new());
        let method_ctx = MethodContext {
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
            auto_registered,
            mcp_tools,
            queue_worker_enabled: opts.spawn_queue_worker,
            rerank_supervisor: rerank_supervisor.clone(),
            control_socket: Some(control_sock_guard.path().to_path_buf()),
            queue_paused: Arc::clone(&queue_paused),
            log_stream: log_stream.clone(),
            plan_registry,
            mounter: Some(Arc::new(crate::mount::Mounter::new(
                Arc::clone(&registry),
                reranker.as_ref().map(|r| r.stage.clone()),
                opts.caller.clone(),
            ))),
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

        let mounted_set = crate::mount::MountedSet::new(Arc::clone(&registry));

        Ok(Self {
            cfg,
            registry,
            info_context,
            log_stream,
            queue_state,
            queue_state_path,
            shutdown_tx,
            runtime_dir,
            lock_path,
            started_at,
            started_at_wall,
            mcp_label,
            mcp_listener,
            event_stream,
            write_guard,
            control_sock,
            signal_triggered,
            queue_paused,
            method_context: method_ctx,
            rerank_supervisor,
            _tty_lock: tty_lock,
            _mounted_set: mounted_set,
            _obs_guard: obs_guard,
            shutdown_rx,
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
            shutdown_rx,
            signal_triggered,
            queue_worker,
            signal_handle,
            control_accept_handle,
            control_sock,
            event_stream,
            rerank_supervisor,
            // Bound (not folded into `..`) so the flocks live across
            // the drain timeouts below; `..` would drop them here.
            _tty_lock,
            _mounted_set,
            ..
        } = self;

        let mut foreground_rx = shutdown_rx;
        let _ = foreground_rx.recv().await;
        tracing::info!("shutdown signalled, joining session tasks");

        // Backstop for a caller that fired the broadcast without going
        // through `initiate_shutdown` — an embedder holding its own
        // `shutdown_tx` clone. It cannot rescue that caller's attached
        // subscribers: connection tasks wake on the same broadcast and
        // flush only what is already queued, so a transition published
        // here has already lost the race. Both in-tree entry points
        // publish ahead of the broadcast instead; the repeat is a no-op.
        event_stream.set_stopping();

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

        // Bounded internally: TERM, the configured grace, then KILL.
        if let Some(supervisor) = rerank_supervisor {
            supervisor.shutdown().await;
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

/// The mount set decided at bring-up.
struct MountPlan {
    /// Libraries to mount, in registry order for the eager form.
    names: Vec<String>,
    /// The registry's `default` pointer as declared; `None` when it
    /// declares none, and for a root the registry does not know.
    default_name: Option<String>,
    /// Whether each name resolves through the registry (`true`) or
    /// the set is the single primary selection (`false`).
    eager: bool,
}

/// Refuse bring-up on a path-class root whose manifest uuid the
/// registry records under a name pointing at another path. A
/// registry-class selection already names its library and is not
/// consulted; a root the registry does not know, or knows at this
/// path, passes.
fn refuse_relocated_identity(cfg: &Config) -> Result<(), crate::backend_probe::PreflightRefusal> {
    if !matches!(
        cfg.source(),
        ResolutionSource::DataDirFlag
            | ResolutionSource::EnvVar
            | ResolutionSource::PortableExeNeighbor
    ) {
        return Ok(());
    }
    match bookrack_config::identify_root(cfg.data_dir()) {
        RootIdentity::UuidElsewhere { name, entry_root } => {
            Err(crate::backend_probe::PreflightRefusal {
                problem: relocated_identity_problem(cfg.data_dir(), &name, &entry_root),
                library: name,
            })
        }
        RootIdentity::Named { .. } | RootIdentity::Unregistered => Ok(()),
    }
}

/// The three-part refusal for a root carrying a registered library's
/// identity at a path the registry does not record: the summary names
/// the root and the library, the detail both paths, the hint the two
/// `libraries add` forms that resolve a move and a copy.
fn relocated_identity_problem(root: &Path, name: &str, entry_root: &Path) -> Problem {
    Problem::new(format!(
        "cannot serve '{}': its manifest identifies library '{name}', which the registry \
         places at another path",
        root.display()
    ))
    .detail(format!(
        "The manifest at '{}' carries the uuid the registry records for '{name}' at '{}'. \
         Serving this root under that name would redirect the daemon to the registered \
         root; serving it anonymously would run one identity in two places.",
        root.display(),
        entry_root.display()
    ))
    .hint(format!(
        "If the library moved, re-register it with `bookrack libraries add {name} {root}`; \
         if this is a copy, give it its own identity with `bookrack libraries add <name> \
         {root} --new-uuid`.",
        root = root.display()
    ))
}

/// Decide which libraries the daemon serves. `primary_name` is the
/// bring-up selection's resolved name; when it matches a registry
/// entry the daemon mounts every entry, otherwise it serves the
/// primary root alone (a path-class root the registry does not know,
/// or a machine with no registry). The default pointer is
/// `registry_default` as the registry declares it — a pointer naming
/// no entry is carried as such, not inferred from the entries or the
/// bring-up selection — and `None` for the single-root form.
fn plan_mounts(
    primary_name: &str,
    entries: &[LibraryEntry],
    registry_default: Option<&str>,
) -> MountPlan {
    if entries.iter().any(|e| e.name == primary_name) {
        MountPlan {
            names: entries.iter().map(|e| e.name.clone()).collect(),
            default_name: registry_default.map(str::to_string),
            eager: true,
        }
    } else {
        MountPlan {
            names: vec![primary_name.to_string()],
            default_name: None,
            eager: false,
        }
    }
}

/// Register `root` as claimed by library `name`, canonicalizing so two
/// registry spellings of one directory collide. Returns the name that
/// already claimed the root, when one did: two entries naming the same
/// root is a registry error the operator must fix, and the second lock
/// acquisition would fail against the first, so the later claimant is
/// skipped up front with both entry names.
fn claim_unique_root(
    seen: &mut HashMap<PathBuf, String>,
    root: &Path,
    name: &str,
) -> Option<String> {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    match seen.get(&canonical) {
        Some(previous) => Some(previous.clone()),
        None => {
            seen.insert(canonical, name.to_string());
            None
        }
    }
}

/// A library bring-up is still trying to serve: its name, its
/// configuration, and its root lock once taken. Dropping a candidate
/// releases the lock.
struct Candidate {
    name: String,
    cfg: Arc<Config>,
    guard: Option<MountGuard>,
}

impl Candidate {
    fn new(name: &str, cfg: Arc<Config>) -> Candidate {
        Candidate {
            name: name.to_string(),
            cfg,
            guard: None,
        }
    }
}

/// Build one library's warm [`LibraryHandle`]: resolve the library's
/// effective embed configuration, construct the books and papers
/// embedding clients, preflight both catalog schemas, open both query
/// libraries, and assemble the [`Ops`] the handle wraps.
///
/// Inputs are one per-library [`Config`] plus process-level shared
/// state: the reranker stage (when the daemon brought one up; it is
/// cloned into the handle's `Ops`) and the [`Caller`] attribution
/// baked into control-socket writes. The handle is registered under
/// `name`. Every handle built here is fully serveable
/// (`Ops::with_library`) — read, ingest, and glean paths all work,
/// unlike an `Ops::catalog_only` handle, whose pipeline entry points
/// report an error.
pub(crate) async fn build_library_handle(
    cfg_arc: Arc<Config>,
    name: &str,
    reranker_stage: Option<&bookrack_ops::RerankStage>,
    caller: Caller,
    mount_guard: Option<MountGuard>,
) -> Result<Arc<LibraryHandle<OllamaEmbedClient>>> {
    let cfg = cfg_arc.as_ref();
    // The embed model resolves per library from the index profile,
    // else the built-in default; an undefined or unloadable profile
    // refuses the handle with the repair spelled out, rather than
    // embedding under a combination the operator never declared.
    let embed_cfg = crate::profile::effective_embed_config(cfg)
        .context("resolve effective embed configuration")?;
    let embedder = OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build embedding client")?;

    // Catalog preflight: migrate each on-disk catalog forward to the
    // binary's `TARGET_VERSION` (with the usual one-shot backup)
    // before the handle serves, then drop the handle so
    // `Library::open` below claims the read-write connection fresh. A
    // read-only check would refuse a database one schema behind the
    // binary even though the migration step is forward-compatible —
    // read-write open lets routine binary upgrades just work.
    if cfg.catalog_db().exists() {
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir())
            .context("preflight catalog schema check failed")?;
    }
    if cfg.papers_catalog_db().exists() {
        Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
            .context("preflight papers catalog schema check failed")?;
    }

    // Library::open — books + papers. Each pipeline owns its
    // chunk-version stamp; papers warms unconditionally so the first
    // glean into an empty data dir lights up the read path the same
    // way the book side does.
    let search_cfg = SearchConfig::resolve(cfg.root_config());
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

    let ops = Ops::with_library(
        library,
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        cfg.books_dir(),
        cfg.backup_dir(),
        caller,
    )
    .with_papers(papers_library, papers_paths);
    let ops = match reranker_stage {
        Some(stage) => ops.with_reranker(stage.clone()),
        None => ops,
    };
    // Job templates resolve from this library's own configuration:
    // its `audit-rules/` overlays, its heading patterns, and the
    // embedding model its index profile declares.
    let templates = JobTemplates {
        ingest: build_queue_params_template(cfg, &embed_cfg),
        glean: build_glean_params_template(cfg, &embed_cfg),
    };
    Ok(LibraryHandle::with_templates(
        name,
        Arc::clone(&cfg_arc),
        ops,
        templates,
        mount_guard,
    ))
}

/// Move a queue snapshot left under a library's data root by an older
/// binary into the daemon state directory. A no-op when the target
/// already exists (the migration ran before) or no legacy document is
/// present. `rename` is tried first; a cross-device rename failure
/// falls back to copy + remove. The job schema is unchanged by the
/// move — each job already carries its target `library`.
fn migrate_queue_snapshot(legacy: &std::path::Path, target: &std::path::Path) -> Result<()> {
    if target.exists() || !legacy.exists() {
        return Ok(());
    }
    if std::fs::rename(legacy, target).is_err() {
        std::fs::copy(legacy, target).with_context(|| {
            format!(
                "copy queue snapshot {} to {}",
                legacy.display(),
                target.display()
            )
        })?;
        std::fs::remove_file(legacy)
            .with_context(|| format!("remove migrated queue snapshot {}", legacy.display()))?;
    }
    tracing::info!(
        from = %legacy.display(),
        to = %target.display(),
        "queue snapshot migrated into the daemon state directory",
    );
    Ok(())
}

/// Build the [`IngestParams`] template the queue worker reuses for
/// every job.
/// Consecutive respawn attempts within one reranker outage after
/// which the backend counts as crash-looping. Below the threshold a
/// restart is routine self-healing; at it, the exponential backoff
/// has already failed twice and the outage deserves operator
/// attention.
// setting: reranker.crashloop_attempts
pub(crate) const RERANK_CRASHLOOP_ATTEMPTS: u32 = 3;

/// Map a supervisor state transition onto the reranker degraded
/// cause: `Some(true)` enters, `Some(false)` exits, `None` leaves the
/// cause untouched. Only `Ready` clears — `Starting` oscillates with
/// `Restarting` inside one outage and must not end the condition.
fn rerank_degraded_transition(state: &SupervisorState) -> Option<bool> {
    match state {
        SupervisorState::Ready => Some(false),
        SupervisorState::Restarting { attempt, .. } if *attempt >= RERANK_CRASHLOOP_ATTEMPTS => {
            Some(true)
        }
        _ => None,
    }
}

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

/// Overlay the per-job overrides on top of the worker's startup
/// [`IngestParams`] template.
///
/// `force` and `hold_for_metadata` come straight off the
/// [`bookrack_core::queue::QueueJob`]. `audit_profile`, when present,
/// triggers a fresh [`load_audit_profile`] for the named built-in so
/// the override actually reaches the pipeline; when absent the
/// template's default profile passes through unchanged.
fn prepare_book_params(
    template: &IngestParams,
    cfg: &Config,
    force: bool,
    hold_for_metadata: bool,
    audit_profile: Option<&str>,
) -> IngestParams {
    let mut params = template.clone();
    params.force = force;
    params.hold_for_metadata = hold_for_metadata;
    if let Some(name) = audit_profile {
        params.audit_profile = load_audit_profile(cfg, Some(name));
    }
    params
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

/// The static half of a library's status card: where it lives, what it
/// is called, how it was resolved, and what it embeds with.
///
/// Derived from one library's [`Config`], so a caller holding a handle
/// builds the card's identity from that library rather than from a
/// bring-up snapshot of the selected one. `mcp_addr` is the exception
/// and is passed in: the listener is a property of the process, not of
/// any library it serves.
pub fn library_info_context(cfg: &Config, embed_model: &str, mcp_addr: &str) -> LibraryInfoContext {
    LibraryInfoContext {
        data_dir: cfg.data_dir().display().to_string(),
        library_name: cfg.library().map(str::to_string),
        resolution_source: resolution_source_label(cfg.source()).to_string(),
        shadowed_default: cfg.shadowed_default().map(|shadowed| {
            format!(
                "registry default '{}' ({}) was not consulted; {} selected the root",
                shadowed.name,
                shadowed.data_dir.display(),
                resolution_source_label(cfg.source())
            )
        }),
        library_identification: cfg
            .library_identification()
            .and_then(library_identification_label)
            .map(str::to_string),
        ollama_url: cfg.ollama_url().to_string(),
        embed_model_configured: embed_model.to_string(),
        mcp_addr: mcp_addr.to_string(),
    }
}

/// The static half of a library's status card, built from the library a
/// handle names: [`library_info_context`] over `cfg`, with the embedding
/// model taken from the library's effective index profile. Every front
/// end serving `library.info` for a selected library builds the card's
/// identity through this one function, so the identity and the counts
/// on one card always describe the same root. `mcp_addr` is the
/// process-wide listener address and is passed in from the bring-up
/// snapshot.
///
/// Fails when the library's index profile does not resolve; the error
/// classifies as caller input through the control-plane error map.
pub fn library_info_context_for(cfg: &Config, mcp_addr: &str) -> Result<LibraryInfoContext> {
    let embed = crate::profile::effective_embed_config(cfg)?;
    Ok(library_info_context(cfg, &embed.model, mcp_addr))
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

/// Render a [`LibraryIdentification`] as the note woven beside the
/// resolution source in `library.info`, or `None` when there is nothing
/// worth surfacing. `Selected` yields `None`: a registry selection is
/// already conveyed by the library name and resolution source, so only a
/// path-class root claimed after the fact carries a note.
pub fn library_identification_label(id: LibraryIdentification) -> Option<&'static str> {
    match id {
        LibraryIdentification::Selected => None,
        LibraryIdentification::ManifestUuid => Some("manifest uuid"),
        LibraryIdentification::Path => Some("path"),
    }
}

/// Aggregate the platform's shutdown signals onto the shared broadcast.
async fn signal_task(
    shutdown_tx: broadcast::Sender<()>,
    triggered: Arc<AtomicBool>,
    event_stream: EventStreamHandle,
) -> Result<()> {
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
    crate::control::events::initiate_shutdown(&event_stream, &shutdown_tx);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_config::LibrarySelection;

    #[test]
    fn rerank_degraded_transition_enters_at_threshold_and_exits_on_ready() {
        let restarting = |attempt| SupervisorState::Restarting {
            attempt,
            next_delay: Duration::from_secs(1),
        };
        let table: [(SupervisorState, Option<bool>); 6] = [
            (SupervisorState::Ready, Some(false)),
            (SupervisorState::Starting, None),
            (restarting(1), None),
            (restarting(RERANK_CRASHLOOP_ATTEMPTS - 1), None),
            (restarting(RERANK_CRASHLOOP_ATTEMPTS), Some(true)),
            (restarting(RERANK_CRASHLOOP_ATTEMPTS + 5), Some(true)),
        ];
        for (state, expected) in table {
            assert_eq!(
                rerank_degraded_transition(&state),
                expected,
                "state: {state:?}"
            );
        }
    }

    /// Build a [`Config`] rooted at a fresh temp directory so the
    /// per-job-override unit tests can call into the audit profile
    /// resolver without depending on the user's data root.
    fn synthetic_cfg() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let selection = LibrarySelection {
            data_dir: Some(dir.path().to_path_buf()),
            library: None,
        };
        let cfg = Config::resolve(&selection).expect("resolve synthetic cfg");
        (cfg, dir)
    }

    fn entry(name: &str, root: &Path, is_default: bool) -> LibraryEntry {
        LibraryEntry {
            name: name.to_string(),
            data_dir: root.to_path_buf(),
            is_default,
            kind: bookrack_config::LibraryKind::Prod,
            description: None,
            index_profile: None,
            created_at: None,
            uuid: None,
        }
    }

    #[test]
    fn plan_mounts_registered_primary_mounts_every_entry() {
        let root = Path::new("root");
        let entries = [entry("a", root, true), entry("b", root, false)];
        let plan = plan_mounts("b", &entries, Some("a"));
        assert!(plan.eager);
        assert_eq!(plan.names, ["a", "b"]);
        // The default pointer starts at the registry's default entry,
        // not at the bring-up selection.
        assert_eq!(plan.default_name.as_deref(), Some("a"));
    }

    #[test]
    fn plan_mounts_unregistered_primary_keeps_the_single_library_form() {
        let root = Path::new("root");
        let entries = [entry("a", root, true)];
        let plan = plan_mounts("default", &entries, Some("a"));
        assert!(!plan.eager);
        assert_eq!(plan.names, ["default"]);
        // A root the registry does not know has no pointer: the only
        // mounted library is the route by the registry's own rule.
        assert_eq!(plan.default_name, None);
    }

    #[test]
    fn plan_mounts_default_is_none_without_a_registry_default() {
        let root = Path::new("root");
        let entries = [entry("a", root, false), entry("b", root, false)];
        let plan = plan_mounts("b", &entries, None);
        assert!(plan.eager);
        assert_eq!(plan.default_name, None);
    }

    /// The pointer is the registry's `default` as written. A pointer
    /// naming no entry is still a declared default: it is carried as
    /// such, so the registry refuses an unnamed call instead of
    /// inferring a default from the entries or the bring-up selection.
    #[test]
    fn plan_mounts_keeps_a_dangling_default() {
        let root = Path::new("root");
        let entries = [entry("beta", root, false)];
        let plan = plan_mounts("beta", &entries, Some("ghost"));
        assert!(plan.eager);
        assert_eq!(plan.default_name.as_deref(), Some("ghost"));
    }

    #[test]
    fn claim_unique_root_rejects_two_entries_on_one_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut seen = HashMap::new();
        assert_eq!(claim_unique_root(&mut seen, dir.path(), "a"), None);
        assert_eq!(
            claim_unique_root(&mut seen, dir.path(), "b").as_deref(),
            Some("a"),
            "the later claimant learns who holds the root"
        );
    }

    #[test]
    fn claim_unique_root_accepts_distinct_roots() {
        let dir_a = tempfile::tempdir().expect("tempdir");
        let dir_b = tempfile::tempdir().expect("tempdir");
        let mut seen = HashMap::new();
        assert_eq!(claim_unique_root(&mut seen, dir_a.path(), "a"), None);
        assert_eq!(claim_unique_root(&mut seen, dir_b.path(), "b"), None);
    }

    #[test]
    fn migrate_queue_snapshot_moves_a_legacy_document_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join(".bookrack-queue.json");
        let target = dir.path().join("state").join("queue.json");
        std::fs::create_dir_all(target.parent().unwrap()).expect("create state dir");
        std::fs::write(&legacy, b"{\"jobs\":[]}").expect("write legacy");

        migrate_queue_snapshot(&legacy, &target).expect("migrate");
        assert!(!legacy.exists(), "legacy document must be moved away");
        assert_eq!(
            std::fs::read(&target).expect("read target"),
            b"{\"jobs\":[]}"
        );
    }

    #[test]
    fn migrate_queue_snapshot_never_clobbers_an_existing_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join(".bookrack-queue.json");
        let target = dir.path().join("queue.json");
        std::fs::write(&legacy, b"legacy").expect("write legacy");
        std::fs::write(&target, b"current").expect("write target");

        migrate_queue_snapshot(&legacy, &target).expect("migrate");
        assert_eq!(std::fs::read(&target).expect("read target"), b"current");
        assert!(
            legacy.exists(),
            "a superseded legacy document is left in place"
        );
    }

    #[test]
    fn migrate_queue_snapshot_is_a_no_op_with_no_legacy_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join(".bookrack-queue.json");
        let target = dir.path().join("queue.json");
        migrate_queue_snapshot(&legacy, &target).expect("migrate");
        assert!(!target.exists());
    }

    #[test]
    fn prepare_book_params_threads_force_and_hold_for_metadata() {
        let (cfg, _dir) = synthetic_cfg();
        let template = build_queue_params_template(&cfg, &EmbedConfig::default());
        let params = prepare_book_params(&template, &cfg, true, true, None);
        assert!(params.force);
        assert!(params.hold_for_metadata);
        // No per-job override: the template's profile passes through.
        assert_eq!(params.audit_profile.name, template.audit_profile.name);
    }

    #[test]
    fn prepare_book_params_overrides_audit_profile_when_set() {
        let (cfg, _dir) = synthetic_cfg();
        let template = build_queue_params_template(&cfg, &EmbedConfig::default());
        let params = prepare_book_params(&template, &cfg, false, false, Some("strict"));
        // The override resolves through `load_audit_profile`, so the
        // result must be the named built-in -- not the template's
        // baseline profile.
        let expected = load_audit_profile(&cfg, Some("strict"));
        assert_eq!(params.audit_profile.name, expected.name);
        assert_ne!(params.audit_profile.name, template.audit_profile.name);
    }
}
