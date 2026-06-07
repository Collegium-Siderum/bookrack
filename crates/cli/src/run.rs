// SPDX-License-Identifier: Apache-2.0

//! `bookrack run` — the daemon-REPL process entry point.
//!
//! One [`run_daemon`] call brings up the session-scoped process: it
//! acquires the machine-wide [`TtyLock`], opens the [`LibraryRegistry`]
//! that every later subsystem routes through, mounts the MCP listener
//! as an in-process task, and joins a coordinated shutdown channel
//! that signal handlers, the REPL, and (later phase) the queue worker
//! all write to.
//!
//! The foreground task is the [`repl_loop`] on `spawn_blocking`:
//! reedline is synchronous, so we keep it off the async runtime and
//! let the underlying OS thread own stdin. The loop reads a line,
//! routes built-in commands directly (`exit`, `use`, `status`, ...),
//! and falls back to `Cli::try_parse_from` for grammar validation;
//! external-subcommand dispatch wires through in phase 5.

use std::borrow::Cow;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, EmbedConfig, LibrarySelection, LogConfig, McpConfig, ResolutionSource, SearchConfig,
};
use bookrack_embed::OllamaEmbedClient;
use bookrack_ingest::IngestParams;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use bookrack_query::Library;
use bookrack_session::{TtyLock, resolve_runtime_dir, tty_lock_name};
use clap::CommandFactory;
use clap::Parser;
use reedline::{
    FileBackedHistory, History, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, Signal,
};
use tokio::sync::broadcast;

use bookrack_cli::queue::{self, Priority, QueueState};

/// CLI inputs for [`run_daemon`]. Parsed from the `Run` clap variant.
pub struct RunOpts {
    /// Library selection forwarded to [`Config::resolve`].
    pub selection: LibrarySelection,
    /// Override the MCP listener address; falls back to [`McpConfig::from_env`].
    pub mcp_addr: Option<SocketAddr>,
    /// Skip binding the MCP listener. The daemon still acquires the
    /// tty lock and opens the registry; useful for development and for
    /// running the daemon on a host where another tool already owns
    /// the MCP port.
    pub no_mcp: bool,
    /// Override the runtime directory. Falls back to [`RUNTIME_DIR_ENV`]
    /// or the platform default. Primarily a test hook so suites can
    /// isolate the tty lock from the operator's session.
    pub runtime_dir: Option<PathBuf>,
}

/// Run the daemon-REPL process to completion.
///
/// Returns once the shutdown broadcast fires (signal, future REPL
/// `exit`, or an MCP listener that bailed out on its own) and every
/// spawned task has joined.
pub async fn run_daemon(opts: RunOpts) -> Result<()> {
    let runtime_dir =
        resolve_runtime_dir(opts.runtime_dir.as_deref()).context("resolve BOOKRACK_RUNTIME_DIR")?;
    std::fs::create_dir_all(&runtime_dir).with_context(|| {
        format!(
            "create runtime directory {} for the bookrack session lock",
            runtime_dir.display()
        )
    })?;

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
    let _tty_lock = TtyLock::acquire(&lock_path, std::process::id(), &mcp_label)?;
    tracing::info!(
        path = %lock_path.display(),
        mcp = %mcp_label,
        "bookrack session lock acquired",
    );

    let cfg = Arc::new(Config::resolve(&opts.selection).context("resolve configuration")?);
    let _obs_guard = bookrack_obs::init(&cfg, &LogConfig::from_env());

    let embed_cfg = EmbedConfig::from_env();
    let embedder = OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build embedding client")?;

    if cfg.catalog_db().exists() {
        Catalog::open_read_only(&cfg.catalog_db())
            .context("preflight catalog schema check failed")?;
    }

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

    let ops = Ops::with_library(
        library,
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        cfg.books_dir(),
        cfg.backup_dir(),
        Caller::cli(),
    );

    let library_name = cfg.library().unwrap_or("default").to_string();
    let handle = LibraryHandle::new(&library_name, ops);
    let registry = LibraryRegistry::single(handle);
    tracing::info!(library = %library_name, "library registry warmed up");

    let info_context = LibraryInfoContext {
        data_dir: cfg.data_dir().display().to_string(),
        library_name: cfg.library().map(str::to_string),
        resolution_source: resolution_source_label(cfg.source()).to_string(),
        ollama_url: cfg.ollama_url().to_string(),
        embed_model_configured: embed_cfg.model.clone(),
    };

    let (shutdown_tx, _) = broadcast::channel::<()>(8);

    let signal_triggered = Arc::new(AtomicBool::new(false));
    let signal_handle = tokio::spawn(signal_task(
        shutdown_tx.clone(),
        Arc::clone(&signal_triggered),
    ));

    // Persistent ingest queue: the file lives under the data root so
    // the next session resumes its pending jobs. The Mutex guards both
    // the REPL command surface (add / cancel / pause) and the worker
    // pull/outcome loop.
    let queue_state_path = cfg.data_dir().join(".bookrack-queue.json");
    let queue_state = Arc::new(Mutex::new(
        queue::load(&queue_state_path).context("load persistent queue state")?,
    ));
    let queue_params_template = build_queue_params_template(&cfg, &embed_cfg);
    let worker_handle = {
        let registry = Arc::clone(&registry);
        let state = Arc::clone(&queue_state);
        let state_path = queue_state_path.clone();
        let params_template = queue_params_template.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        let library_default = library_name.clone();
        tokio::spawn(queue::worker_loop(
            state_path,
            state,
            shutdown_rx,
            move |job| {
                let registry = Arc::clone(&registry);
                let params_template = params_template.clone();
                let library_default = library_default.clone();
                // The ingest body holds non-Send Catalog / Corpus
                // handles across `.await`, so the future is not Send.
                // Park the whole call on a blocking worker and drive
                // it with `Handle::block_on`: the outer JoinHandle is
                // Send, and the blocking thread keeps the !Send chain
                // off the runtime workers.
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
        ))
    };

    let mcp_handle = match mcp_addr {
        Some(addr) => {
            let registry = Arc::clone(&registry);
            let rx = shutdown_tx.subscribe();
            Some(tokio::spawn(async move {
                bookrack_mcp::serve(registry, info_context, &addr, rx).await
            }))
        }
        None => {
            tracing::info!("MCP listener disabled (--no-mcp); session running without /mcp");
            None
        }
    };

    // Foreground task: reedline REPL on a `spawn_blocking` thread. The
    // synchronous read_line blocks an OS thread — keeping it off the
    // async runtime is critical (a parked async task would starve the
    // signal listener and the MCP server).
    let mcp_label_for_repl = mcp_label.clone();
    let lock_path_for_repl = lock_path.clone();
    let runtime_dir_for_repl = runtime_dir.clone();
    let registry_for_repl = Arc::clone(&registry);
    let shutdown_tx_for_repl = shutdown_tx.clone();
    let queue_state_for_repl = Arc::clone(&queue_state);
    let queue_path_for_repl = queue_state_path.clone();
    let library_default_for_repl = library_name.clone();
    let cfg_for_repl = Arc::clone(&cfg);
    let started_at = Instant::now();
    let repl_handle = tokio::task::spawn_blocking(move || {
        repl_loop(
            registry_for_repl,
            shutdown_tx_for_repl,
            runtime_dir_for_repl,
            lock_path_for_repl,
            mcp_label_for_repl,
            queue_state_for_repl,
            queue_path_for_repl,
            library_default_for_repl,
            cfg_for_repl,
            started_at,
        )
    });

    // Wait for any subscriber to signal shutdown — the REPL on
    // `exit` / `Ctrl-D`, the signal listener on SIGINT / SIGTERM /
    // SIGHUP, or the MCP task if it bails out on its own.
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
    match tokio::time::timeout(Duration::from_secs(3), worker_handle).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(err))) => tracing::warn!(error = %err, "queue worker returned error"),
        Ok(Err(err)) => tracing::warn!(error = %err, "queue worker join failed"),
        Err(_) => tracing::warn!("queue worker did not exit within 3s; abandoning"),
    }
    signal_handle.abort();
    // REPL thread may still be blocked on read_line if shutdown came
    // from a signal — abort the join handle (the OS thread is reaped
    // on process exit) and don't wait for it.
    repl_handle.abort();

    // Signal-driven shutdown leaves the reedline thread parked inside
    // its sync `read_line`, holding a blocking-pool worker the runtime
    // cannot cancel. Letting `main` return would have tokio's Runtime
    // drop wait on that worker forever (observed under SIGTERM and
    // SIGHUP). `std::process::exit` skips the Runtime drop and lets
    // the OS reap the blocking thread when the process tears down.
    // REPL-driven `exit` / Ctrl-D returns normally because read_line
    // has already returned and the blocking worker is idle.
    if signal_triggered.load(Ordering::SeqCst) {
        std::process::exit(0);
    }
    Ok(())
}

/// Build the [`IngestParams`] template the queue worker reuses for
/// every job. Per-job toggles like `force` are overwritten inside the
/// runner closure; everything else — the embed model knobs, the audit
/// data overlay, the heading patterns, the active profile — is loaded
/// once at daemon startup so the worker does not re-read the data root
/// on every job.
fn build_queue_params_template(cfg: &Config, embed_cfg: &EmbedConfig) -> IngestParams {
    IngestParams {
        embed: embed_cfg.clone(),
        hold_for_metadata: false,
        force: false,
        audit_data: crate::audit_helpers::load_audit_data(cfg),
        audit_profile: crate::audit_helpers::load_audit_profile(cfg, None),
        heading_patterns: crate::audit_helpers::load_heading_patterns(cfg),
        ..Default::default()
    }
}

fn resolution_source_label(source: ResolutionSource) -> &'static str {
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
///
/// Unix listens for SIGINT, SIGTERM, and SIGHUP — the third covers
/// the "terminal window closed" path, which is the primary way the
/// session ends today. Windows listens for Ctrl-C and the close event.
///
/// Sets `triggered` to `true` before forwarding the broadcast so the
/// caller can distinguish a signal-driven shutdown — where the
/// blocking reedline thread is still parked inside its sync read_line
/// and the tokio Runtime drop would wait for it forever — from a
/// REPL-driven `exit` / Ctrl-D path that returns cleanly.
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

// ---------------------------------------------------------------------------
// REPL surface
// ---------------------------------------------------------------------------

const HISTORY_FILE: &str = ".bookrack-history";
const HISTORY_CAPACITY: usize = 1000;

/// Reedline [`Prompt`] backed by the live registry: the prompt label
/// reads the current default-library name on every render, so a
/// `use <lib>` change is visible on the next prompt line without any
/// repaint plumbing.
struct BookrackPrompt {
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
}

impl Prompt for BookrackPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let name = self
            .registry
            .default_name()
            .unwrap_or_else(|_| "?".to_string());
        Cow::Owned(format!("bookrack:{name}"))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("› ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(":: ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({prefix}reverse-i-search: '{}') ",
            history_search.term
        ))
    }
}

/// What the REPL should do after evaluating one input line.
#[derive(Debug, PartialEq, Eq)]
enum ReplOutcome {
    /// Stay in the REPL, render the prompt again.
    Continue,
    /// Leave the REPL — the user typed `exit` / `quit`. The caller
    /// signals shutdown on the shared broadcast.
    Exit,
}

/// The REPL main loop.
///
/// Runs synchronously on a [`tokio::task::spawn_blocking`] worker so
/// reedline's blocking stdin reads never park the async runtime.
#[allow(clippy::too_many_arguments)]
fn repl_loop(
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    shutdown_tx: broadcast::Sender<()>,
    runtime_dir: PathBuf,
    lock_path: PathBuf,
    mcp_label: String,
    queue_state: Arc<Mutex<QueueState>>,
    queue_state_path: PathBuf,
    library_default: String,
    cfg: Arc<Config>,
    started_at: Instant,
) -> Result<()> {
    let history_path = runtime_dir.join(HISTORY_FILE);
    let history: Box<dyn History> = match FileBackedHistory::with_file(
        HISTORY_CAPACITY,
        history_path.clone(),
    ) {
        Ok(h) => Box::new(h),
        Err(err) => {
            eprintln!(
                "bookrack: history file {} unavailable ({err}); session running without history",
                history_path.display()
            );
            Box::<FileBackedHistory>::default()
        }
    };
    let mut editor = Reedline::create().with_history(history);
    let prompt = BookrackPrompt {
        registry: Arc::clone(&registry),
    };
    let mut shutdown_rx = shutdown_tx.subscribe();

    print_startup_banner(&registry, &lock_path, &mcp_label);

    loop {
        // Drain a shutdown that arrived while the previous command was
        // running, before we block on read_line again.
        if shutdown_rx.try_recv().is_ok() {
            break;
        }
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match handle_line(
                    trimmed,
                    &registry,
                    &lock_path,
                    &mcp_label,
                    &queue_state,
                    &queue_state_path,
                    &library_default,
                    &cfg,
                    started_at,
                ) {
                    ReplOutcome::Continue => {}
                    ReplOutcome::Exit => {
                        let _ = shutdown_tx.send(());
                        break;
                    }
                }
            }
            Ok(Signal::CtrlD) => {
                println!();
                let _ = shutdown_tx.send(());
                break;
            }
            Ok(Signal::CtrlC) => {
                println!("^C  (type `exit` or Ctrl-D to leave)");
                continue;
            }
            Ok(_) => continue,
            Err(err) => {
                eprintln!("bookrack: REPL read_line error: {err}");
                let _ = shutdown_tx.send(());
                break;
            }
        }
    }
    Ok(())
}

/// Print the session header — version, registered libraries, MCP
/// listener address, lock path. Called once before the REPL takes
/// over stdin; afterwards the lines scroll up into the terminal
/// scrollback naturally.
fn print_startup_banner(
    registry: &Arc<LibraryRegistry<OllamaEmbedClient>>,
    lock_path: &Path,
    mcp_label: &str,
) {
    let version = env!("CARGO_PKG_VERSION");
    let libs = registry.list().unwrap_or_default();
    let lib_line = libs
        .iter()
        .map(|s| {
            if s.is_default {
                format!("{} (default)", s.name)
            } else {
                s.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    println!("╭──────────────────────────────────────────────────────────────╮");
    println!("│  bookrack v{version}");
    println!("│  libraries: {lib_line}");
    println!("│  MCP        {mcp_label}");
    println!("│  lock       {}", lock_path.display());
    println!("╰──────────────────────────────────────────────────────────────╯");
    println!(" Type `help` for commands, `exit` or Ctrl-D to leave.");
    println!();
}

/// Evaluate one tokenised input line. Built-in commands (`exit`,
/// `help`, `status`, `libs`, `use`, `queue`) execute directly;
/// anything else is fed through [`ReplCli::try_parse_from`] so the
/// user sees the grammar's actual error messages and dispatch maps
/// each variant to the matching `cmd::*` runner. Async dispatches go
/// through `tokio::runtime::Handle::current().block_on(...)` because
/// the REPL itself runs synchronously on a `spawn_blocking` worker.
#[allow(clippy::too_many_arguments)]
fn handle_line(
    line: &str,
    registry: &Arc<LibraryRegistry<OllamaEmbedClient>>,
    lock_path: &Path,
    mcp_label: &str,
    queue_state: &Arc<Mutex<QueueState>>,
    queue_state_path: &Path,
    library_default: &str,
    cfg: &Arc<Config>,
    started_at: Instant,
) -> ReplOutcome {
    let tokens = match shlex::split(line) {
        Some(tokens) if !tokens.is_empty() => tokens,
        Some(_) => return ReplOutcome::Continue,
        None => {
            eprintln!("bookrack: cannot parse input (unclosed quote?)");
            return ReplOutcome::Continue;
        }
    };

    let head = tokens[0].clone();
    match head.as_str() {
        "exit" | "quit" => return ReplOutcome::Exit,
        "help" => {
            print_repl_help();
            return ReplOutcome::Continue;
        }
        "status" => {
            print_status(registry, lock_path, mcp_label, started_at);
            return ReplOutcome::Continue;
        }
        "libs" => {
            print_libraries(registry);
            return ReplOutcome::Continue;
        }
        "use" => {
            handle_use(registry, &tokens);
            return ReplOutcome::Continue;
        }
        "queue" => {
            handle_queue(queue_state, queue_state_path, library_default, &tokens);
            return ReplOutcome::Continue;
        }
        _ => {}
    }

    // Not a built-in. Parse against the REPL grammar — the in-session
    // write surface for ingest, intake, metadata edits, vectors/corpus
    // rebuilds, stamp reconciliation, and remove.
    match crate::ReplCli::try_parse_from(&tokens) {
        Ok(repl_cli) => execute_repl_command(repl_cli.command, cfg),
        Err(err) => {
            let _ = err.print();
        }
    }
    ReplOutcome::Continue
}

/// Dispatch a parsed [`crate::ReplCommand`] to the matching `cmd::*`
/// runner. Async runners are driven on the current runtime through
/// `Handle::block_on`; the REPL thread is a `spawn_blocking` worker
/// so blocking the calling thread does not stall the async runtime.
fn execute_repl_command(command: crate::ReplCommand, cfg: &Arc<Config>) {
    use crate::ReplCommand;
    use tokio::runtime::Handle;
    let rt = Handle::current();
    let cfg_ref: &Config = cfg.as_ref();
    let result: anyhow::Result<()> = match command {
        ReplCommand::Ingest {
            path,
            recursive,
            hold_for_metadata,
            force,
        } => rt.block_on(crate::cmd::ingest::run(
            cfg_ref,
            &path,
            recursive,
            hold_for_metadata,
            force,
            None,
        )),
        ReplCommand::Intake { action } => match action {
            crate::IntakeAction::Ocr {
                ocr_md,
                from_pdf,
                expected_pages,
                allow_partial,
            } => rt.block_on(crate::cmd::intake_ocr::run(
                cfg_ref,
                &ocr_md,
                &from_pdf,
                expected_pages,
                allow_partial,
                None,
            )),
        },
        ReplCommand::Metadata { action } => {
            rt.block_on(crate::cmd::metadata::run_write(cfg_ref, action, None))
        }
        ReplCommand::Vectors { action } => match action {
            crate::WriteVectorsAction::Rebuild {
                kind,
                num_partitions,
                num_sub_vectors,
                num_bits,
                nprobes,
                refine_factor,
            } => rt.block_on(crate::cmd::vectors::rebuild(
                cfg_ref,
                kind.as_deref(),
                num_partitions,
                num_sub_vectors,
                num_bits,
                nprobes,
                refine_factor,
            )),
            crate::WriteVectorsAction::Drop => rt.block_on(crate::cmd::vectors::drop(cfg_ref)),
            crate::WriteVectorsAction::Reembed {
                book,
                stale_only,
                dry_run,
                yes,
            } => rt.block_on(crate::cmd::vectors::reembed(
                cfg_ref, book, stale_only, dry_run, yes, None,
            )),
        },
        ReplCommand::Corpus { action } => match action {
            crate::CorpusAction::Rebuild {
                include_vectors,
                book,
                stale_only,
                dry_run,
                yes,
            } => rt.block_on(crate::cmd::corpus::rebuild(
                cfg_ref,
                include_vectors,
                book,
                stale_only,
                dry_run,
                yes,
                None,
            )),
        },
        ReplCommand::Stamps { action } => match action {
            crate::StampsAction::Reconcile => rt.block_on(crate::cmd::stamps::reconcile(cfg_ref)),
        },
        ReplCommand::Remove {
            intake_id,
            sha,
            dry_run,
            yes,
        } => rt.block_on(crate::cmd::remove::run(
            cfg_ref,
            crate::cmd::remove::RemoveArgs {
                intake_id,
                sha,
                dry_run,
                yes,
            },
        )),
        ReplCommand::Dryrun {
            path,
            out,
            stdout,
            no_chunk,
        } => crate::cmd::dryrun::run(cfg_ref, &path, out.as_deref(), stdout, no_chunk, None),
    };
    if let Err(err) = result {
        eprintln!("bookrack: {err:#}");
    }
}

fn handle_use(registry: &Arc<LibraryRegistry<OllamaEmbedClient>>, tokens: &[String]) {
    if tokens.len() != 2 {
        eprintln!("bookrack: usage: use <library>");
        return;
    }
    let name = &tokens[1];
    match registry.set_default(name) {
        Ok(()) => println!("default → {name}"),
        Err(err) => eprintln!("bookrack: {err}"),
    }
}

/// Dispatch the REPL's `queue` subcommand. The persistent state is
/// always mutated under the shared lock and persisted before the
/// guard drops, so an `exit` immediately after `queue add` leaves a
/// consistent file for the next session to resume.
fn handle_queue(
    state: &Arc<Mutex<QueueState>>,
    state_path: &Path,
    library_default: &str,
    tokens: &[String],
) {
    let sub = match tokens.get(1) {
        Some(s) => s.as_str(),
        None => {
            print_queue_usage();
            return;
        }
    };
    match sub {
        "add" => queue_add(state, state_path, library_default, &tokens[2..]),
        "list" | "ls" => queue_list(state),
        "cancel" => queue_cancel(state, state_path, &tokens[2..]),
        "clear" => queue_clear(state, state_path),
        "pause" => queue_set_paused(state, state_path, true),
        "resume" => queue_set_paused(state, state_path, false),
        other => {
            eprintln!("bookrack: unknown queue subcommand {other:?}");
            print_queue_usage();
        }
    }
}

fn print_queue_usage() {
    eprintln!("usage:");
    eprintln!("  queue add <path> [--library X] [--priority low|normal|high] [--force]");
    eprintln!("  queue list");
    eprintln!("  queue cancel <id-prefix>");
    eprintln!("  queue clear");
    eprintln!("  queue pause | resume");
}

fn queue_add(
    state: &Arc<Mutex<QueueState>>,
    state_path: &Path,
    library_default: &str,
    args: &[String],
) {
    let mut path_arg: Option<PathBuf> = None;
    let mut library = library_default.to_string();
    let mut priority = Priority::Normal;
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--library" => match args.get(i + 1) {
                Some(v) => {
                    library = v.clone();
                    i += 2;
                }
                None => {
                    eprintln!("bookrack: --library requires a value");
                    return;
                }
            },
            "--priority" => match args.get(i + 1).map(String::as_str) {
                Some("low") => {
                    priority = Priority::Low;
                    i += 2;
                }
                Some("normal") => {
                    priority = Priority::Normal;
                    i += 2;
                }
                Some("high") => {
                    priority = Priority::High;
                    i += 2;
                }
                Some(other) => {
                    eprintln!("bookrack: unknown priority {other:?}");
                    return;
                }
                None => {
                    eprintln!("bookrack: --priority requires a value");
                    return;
                }
            },
            "--force" => {
                force = true;
                i += 1;
            }
            other if other.starts_with("--") => {
                eprintln!("bookrack: unknown flag {other:?}");
                return;
            }
            other => {
                if path_arg.is_some() {
                    eprintln!("bookrack: queue add takes one path; got extra {other:?}");
                    return;
                }
                path_arg = Some(PathBuf::from(other));
                i += 1;
            }
        }
    }
    let Some(path) = path_arg else {
        eprintln!("bookrack: queue add requires a path");
        return;
    };
    let resolved = match path.canonicalize() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("bookrack: resolve {}: {err}", path.display());
            return;
        }
    };
    let paths = if resolved.is_dir() {
        match queue::collect_supported_files(&resolved) {
            Ok(paths) => paths,
            Err(err) => {
                eprintln!("bookrack: walk {}: {err}", resolved.display());
                return;
            }
        }
    } else {
        vec![resolved]
    };
    if paths.is_empty() {
        println!("queue add: no supported files");
        return;
    }
    let count = paths.len();
    let ids = {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(err) => {
                eprintln!("bookrack: queue state mutex poisoned: {err}");
                return;
            }
        };
        let ids = queue::enqueue_files(&mut guard, &paths, &library, priority, force);
        if let Err(err) = queue::save_atomic(&guard, state_path) {
            eprintln!("bookrack: persist queue state: {err}");
            return;
        }
        ids
    };
    println!("queue add: enqueued {count} job(s)");
    for id in ids {
        let short: String = id.chars().take(8).collect();
        println!("  {short}");
    }
}

fn queue_list(state: &Arc<Mutex<QueueState>>) {
    let snapshot = match state.lock() {
        Ok(g) => g.clone(),
        Err(err) => {
            eprintln!("bookrack: queue state mutex poisoned: {err}");
            return;
        }
    };
    print!("{}", queue::render_list(&snapshot));
}

fn queue_cancel(state: &Arc<Mutex<QueueState>>, state_path: &Path, args: &[String]) {
    let Some(prefix) = args.first() else {
        eprintln!("bookrack: queue cancel requires an id prefix");
        return;
    };
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(err) => {
            eprintln!("bookrack: queue state mutex poisoned: {err}");
            return;
        }
    };
    match queue::cancel_with_prefix(&mut guard, prefix) {
        Ok(id) => {
            if let Err(err) = queue::save_atomic(&guard, state_path) {
                eprintln!("bookrack: persist queue state: {err}");
                return;
            }
            println!("queue cancel: {id}");
        }
        Err(err) => eprintln!("bookrack: {err}"),
    }
}

fn queue_clear(state: &Arc<Mutex<QueueState>>, state_path: &Path) {
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(err) => {
            eprintln!("bookrack: queue state mutex poisoned: {err}");
            return;
        }
    };
    let n = queue::cancel_all_pending(&mut guard);
    if let Err(err) = queue::save_atomic(&guard, state_path) {
        eprintln!("bookrack: persist queue state: {err}");
        return;
    }
    println!("queue clear: cancelled {n} pending job(s)");
}

fn queue_set_paused(state: &Arc<Mutex<QueueState>>, state_path: &Path, paused: bool) {
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(err) => {
            eprintln!("bookrack: queue state mutex poisoned: {err}");
            return;
        }
    };
    let prev = std::mem::replace(&mut guard.paused, paused);
    if let Err(err) = queue::save_atomic(&guard, state_path) {
        eprintln!("bookrack: persist queue state: {err}");
        return;
    }
    let _ = prev;
    let label = if paused { "paused" } else { "running" };
    println!("queue: {label}");
}

fn print_repl_help() {
    println!("Built-in commands:");
    println!("  exit, quit       Leave the bookrack session");
    println!("  help             Show this help");
    println!("  status           Show session pid, uptime, libraries, MCP address");
    println!("  libs             List all registered libraries");
    println!("  use <lib>        Switch the default library");
    println!("  queue add <path> [--library X] [--priority {{low|normal|high}}] [--force]");
    println!("                   Enqueue a file or every supported file under a directory");
    println!("  queue list       Show the persistent ingest queue");
    println!("  queue cancel <id-prefix> | clear | pause | resume");
    println!();
    println!("Other subcommands are parsed against the bookrack CLI grammar;");
    println!("execution from the REPL is wired in a later phase.");
    println!();
    let mut cmd = crate::Cli::command();
    let _ = cmd.print_long_help();
    println!();
}

fn print_status(
    registry: &Arc<LibraryRegistry<OllamaEmbedClient>>,
    lock_path: &Path,
    mcp_label: &str,
    started_at: Instant,
) {
    let pid = std::process::id();
    let uptime = format_uptime(started_at.elapsed());
    let default = registry.default_name().unwrap_or_else(|_| "?".to_string());

    println!("session   pid={pid}  uptime={uptime}");
    println!("lock      {}", lock_path.display());
    println!("mcp       {mcp_label}");
    println!("default   {default}");
    print!("libraries");
    match registry.list() {
        Ok(libs) => {
            for s in &libs {
                let marker = if s.is_default { "*" } else { " " };
                print!("  {marker}{}", s.name);
            }
            println!();
        }
        Err(err) => {
            println!();
            eprintln!("bookrack: list libraries: {err}");
        }
    }
}

fn print_libraries(registry: &Arc<LibraryRegistry<OllamaEmbedClient>>) {
    match registry.list() {
        Ok(libs) => {
            for s in libs {
                let marker = if s.is_default { "*" } else { " " };
                let dim = match s.dimension {
                    Some(d) => format!("  dim {d}"),
                    None => String::new(),
                };
                println!(" {marker} {}{dim}", s.name);
            }
        }
        Err(err) => eprintln!("bookrack: {err}"),
    }
}

fn format_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uptime_renders_hours_minutes_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_uptime(Duration::from_secs(59)), "00:00:59");
        assert_eq!(format_uptime(Duration::from_secs(60)), "00:01:00");
        assert_eq!(format_uptime(Duration::from_secs(3_600)), "01:00:00");
        assert_eq!(format_uptime(Duration::from_secs(3_725)), "01:02:05");
        assert_eq!(
            format_uptime(Duration::from_secs(36 * 3_600 + 17 * 60 + 9)),
            "36:17:09"
        );
    }
}
