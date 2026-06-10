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
use bookrack_config::{Config, LibrarySelection, LogConfig};
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::Caller;
use bookrack_ops::registry::LibraryRegistry;
use bookrack_runtime::control::HealthProbe;
use bookrack_runtime::control::events::{Event, EventStreamHandle};
use bookrack_runtime::queue::{self, Priority, QueueState};
use bookrack_runtime::{DaemonRuntime, LaunchMode, RuntimeOpts};
use clap::CommandFactory;
use clap::Parser;
use reedline::{
    FileBackedHistory, History, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, Signal,
};
use serde_json::Value;
use tokio::sync::broadcast;

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
    /// Re-enable the in-process reedline REPL for the lifetime of the
    /// daemon. Default is `false`: the daemon owns no stdin and runs
    /// headless; the operator opens an interactive REPL with
    /// `bookrack repl` in another process.
    pub legacy_repl: bool,
}

pub async fn run_daemon(opts: RunOpts) -> Result<()> {
    let runtime_dir = bookrack_session::resolve_runtime_dir(opts.runtime_dir.as_deref())
        .context("resolve BOOKRACK_RUNTIME_DIR")?;
    let lock_path = runtime_dir.join(bookrack_session::tty_lock_name());

    let runtime_opts = RuntimeOpts {
        selection: opts.selection,
        runtime_dir: opts.runtime_dir,
        mcp_addr: opts.mcp_addr,
        no_mcp: opts.no_mcp,
        spawn_queue_worker: true,
        log_config: LogConfig::from_env(),
        caller: Caller::cli(),
        mcp_tools: bookrack_mcp::list_tools(),
        launch_mode: LaunchMode::Cli,
    };

    let runtime = match DaemonRuntime::start(runtime_opts).await {
        Ok(rt) => rt,
        Err(err) => {
            if bookrack_session::is_lock_conflict(&err) {
                return handle_lock_conflict(err, &lock_path, LaunchMode::Cli).await;
            }
            return Err(err);
        }
    };

    println!(
        "bookrack daemon running: pid={} mcp={} control_sock={}",
        std::process::id(),
        runtime.mcp_label,
        runtime.control_sock.path.display(),
    );
    println!("stop with Ctrl-C or `bookrack quit`; interactive REPL: `bookrack repl`");

    let mcp_handle = bookrack_mcp::spawn_listener(&runtime);
    let repl_handle = spawn_repl_if_tty(&runtime, opts.legacy_repl);

    runtime.run_until_shutdown(mcp_handle, repl_handle).await
}

/// Spawn the foreground task. The default is an async task that
/// resolves on the shutdown broadcast (the daemon owns no stdin and
/// runs headless); a blocking thread here would stall the tokio
/// runtime's teardown, leaving the process alive after a
/// control-plane `daemon.shutdown` has already drained everything.
/// `--legacy-repl` re-enables the in-process reedline REPL for one
/// transition release so the CI scripts that fed REPL via stdin have
/// a window to migrate to `bookrack repl`. The synchronous read_line
/// blocks an OS thread — keeping it off the async runtime is critical.
fn spawn_repl_if_tty(
    runtime: &DaemonRuntime,
    legacy_repl: bool,
) -> tokio::task::JoinHandle<Result<()>> {
    if legacy_repl && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mcp_label = runtime.mcp_label.clone();
        let lock_path = runtime.lock_path.clone();
        let runtime_dir = runtime.runtime_dir.clone();
        let registry = Arc::clone(&runtime.registry);
        let shutdown_tx = runtime.shutdown_tx.clone();
        let queue_state = Arc::clone(&runtime.queue_state);
        let queue_state_path = runtime.queue_state_path.clone();
        let queue_paused = Arc::clone(&runtime.queue_paused);
        let event_stream = runtime.event_stream.clone();
        let library_default = runtime.cfg.library().unwrap_or("default").to_string();
        let cfg = Arc::clone(&runtime.cfg);
        let started_at = runtime.started_at;
        let log_stream = runtime.log_stream.clone();
        tokio::task::spawn_blocking(move || {
            repl_loop(
                registry,
                shutdown_tx,
                runtime_dir,
                lock_path,
                mcp_label,
                queue_state,
                queue_state_path,
                queue_paused,
                event_stream,
                library_default,
                cfg,
                started_at,
                log_stream,
            )
        })
    } else {
        if !legacy_repl {
            tracing::info!(
                "daemon running headless; open an interactive REPL with `bookrack repl`."
            );
        } else {
            tracing::info!(
                "stdin is not a TTY; running headless. Stop with a signal or the \
                 `session.shutdown` MCP tool.",
            );
        }
        let mut shutdown_rx = runtime.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let _ = shutdown_rx.recv().await;
            anyhow::Ok(())
        })
    }
}

/// Resolve a session-lock conflict by probing the running daemon and
/// taking the action that matches the entry point. `LaunchMode::Cli`
/// prints the recorded pid and control socket and exits zero so a
/// second `bookrack run` invocation is a no-op; `LaunchMode::Gui`
/// routes a `tray.focus` RPC at the live daemon and exits zero so a
/// double-launched GUI raises its existing window. A lock pointing at
/// a dead daemon exits with status 3; an unprobeable lock (no
/// `control_sock=` recorded) falls back to surfacing the original
/// acquire error.
async fn handle_lock_conflict(
    err: anyhow::Error,
    lock_path: &Path,
    mode: LaunchMode,
) -> Result<()> {
    let info = match bookrack_session::peek_lock(lock_path) {
        Ok(Some(i)) => i,
        Ok(None) | Err(_) => {
            eprintln!("{err:#}");
            std::process::exit(1);
        }
    };
    let probe = bookrack_runtime::control::probe(&info, Duration::from_secs(2)).await;
    match (mode, probe) {
        (LaunchMode::Cli, HealthProbe::Healthy(pid, sock)) => {
            println!(
                "bookrack daemon already running: pid={pid} control_sock={}",
                sock.display()
            );
            std::process::exit(0);
        }
        (LaunchMode::Gui, HealthProbe::Healthy(_pid, sock)) => {
            let socket = bookrack_control_client::ControlSocket::from_path(sock);
            let client = bookrack_control_client::connect(&socket)
                .await
                .context("connect to live daemon control socket for tray.focus")?;
            let _: Value = client
                .call("tray.focus", Value::Null)
                .await
                .context("tray.focus rpc")?;
            std::process::exit(0);
        }
        (_, HealthProbe::Stale) => {
            eprintln!(
                "bookrack session lock at {} is stale (no live daemon answered within 2s).",
                lock_path.display()
            );
            eprintln!(
                "Remove the lock file manually and re-run bookrack: rm {}",
                lock_path.display()
            );
            std::process::exit(3);
        }
        (_, HealthProbe::Unprobeable) => {
            eprintln!("{err:#}");
            std::process::exit(1);
        }
    }
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
    queue_paused: Arc<AtomicBool>,
    event_stream: EventStreamHandle,
    library_default: String,
    cfg: Arc<Config>,
    started_at: Instant,
    log_stream: LogStreamHandle,
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
                    &queue_paused,
                    &event_stream,
                    &library_default,
                    &cfg,
                    started_at,
                    &log_stream,
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
    queue_paused: &Arc<AtomicBool>,
    event_stream: &EventStreamHandle,
    library_default: &str,
    cfg: &Arc<Config>,
    started_at: Instant,
    log_stream: &LogStreamHandle,
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
            handle_queue(
                queue_state,
                queue_state_path,
                queue_paused,
                event_stream,
                library_default,
                &tokens,
            );
            return ReplOutcome::Continue;
        }
        "logs" => {
            handle_logs(log_stream, &tokens);
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
        ReplCommand::Ingest(args) => rt.block_on(bookrack_runtime::cmd::ingest::run(
            cfg_ref,
            &args.path,
            args.recursive,
            args.hold_for_metadata,
            args.force,
            None,
        )),
        ReplCommand::Intake { action } => match action {
            crate::IntakeAction::Ocr {
                ocr_md,
                from_pdf,
                expected_pages,
                allow_partial,
            } => rt.block_on(bookrack_runtime::cmd::intake_ocr::run(
                cfg_ref,
                &ocr_md,
                &from_pdf,
                expected_pages,
                allow_partial,
                None,
            )),
        },
        ReplCommand::Metadata { action } => rt.block_on(
            bookrack_runtime::cmd::metadata::run_write(cfg_ref, action, None),
        ),
        ReplCommand::Vectors { action } => match action {
            crate::WriteVectorsAction::Rebuild {
                kind,
                num_partitions,
                num_sub_vectors,
                num_bits,
                nprobes,
                refine_factor,
            } => rt.block_on(bookrack_runtime::cmd::vectors::rebuild(
                cfg_ref,
                kind.as_deref(),
                num_partitions,
                num_sub_vectors,
                num_bits,
                nprobes,
                refine_factor,
            )),
            crate::WriteVectorsAction::Drop => {
                rt.block_on(bookrack_runtime::cmd::vectors::drop(cfg_ref))
            }
            crate::WriteVectorsAction::Reembed {
                book,
                stale_only,
                dry_run,
                yes,
            } => rt.block_on(bookrack_runtime::cmd::vectors::reembed(
                cfg_ref,
                book,
                stale_only,
                dry_run,
                yes,
                None,
                crate::util::confirm,
            )),
            crate::WriteVectorsAction::Reset { yes, resume } => rt.block_on(
                bookrack_runtime::cmd::vectors::reset(cfg_ref, yes, resume, |prompt| {
                    crate::util::confirm_token(prompt, "RESET")
                }),
            ),
        },
        ReplCommand::Corpus { action } => match action {
            crate::CorpusAction::Rebuild {
                include_vectors,
                book,
                stale_only,
                dry_run,
                yes,
            } => rt.block_on(bookrack_runtime::cmd::corpus::rebuild(
                cfg_ref,
                include_vectors,
                book,
                stale_only,
                dry_run,
                yes,
                None,
                crate::util::confirm,
            )),
        },
        ReplCommand::Stamps { action } => match action {
            crate::StampsAction::Reconcile => {
                rt.block_on(bookrack_runtime::cmd::stamps::reconcile(cfg_ref))
            }
        },
        ReplCommand::Remove(args) => rt.block_on(bookrack_runtime::cmd::remove::run(
            cfg_ref,
            bookrack_runtime::cmd::remove::RemoveArgs {
                intake_id: args.intake_id,
                sha: args.sha,
                dry_run: args.dry_run,
                yes: args.yes,
            },
        )),
        ReplCommand::Dryrun(args) => bookrack_runtime::cmd::dryrun::run(
            cfg_ref,
            &args.path,
            args.out.as_deref(),
            args.stdout,
            args.no_chunk,
            None,
        ),
        ReplCommand::Queue { .. } => {
            // The REPL intercepts `queue` ahead of the clap grammar so
            // the worker-loop pause flag and event stream stay in scope.
            unreachable!("queue subcommand is handled by handle_queue");
        }
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

fn handle_queue(
    state: &Arc<Mutex<QueueState>>,
    state_path: &Path,
    queue_paused: &Arc<AtomicBool>,
    event_stream: &EventStreamHandle,
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
        "clear" => queue_clear(state, state_path, queue_paused, event_stream),
        "pause" => queue_set_paused(state, state_path, queue_paused, event_stream, true),
        "resume" => queue_set_paused(state, state_path, queue_paused, event_stream, false),
        other => {
            eprintln!("bookrack: unknown queue subcommand {other:?}");
            print_queue_usage();
        }
    }
}

/// Default `n` for the REPL `logs tail` built-in.
const LOGS_TAIL_REPL_DEFAULT: usize = 50;

/// REPL `logs` built-in. The only sub-command is
/// `logs tail [<n>]`: print the most recent `n` events from the
/// in-process ring buffer (default [`LOGS_TAIL_REPL_DEFAULT`]).
///
/// This is the in-process fast path counterpart to
/// `bookrack exec logs tail`: it reads the [`LogStreamHandle`]
/// directly instead of round-tripping through MCP, so events appear
/// even when the listener is `--no-mcp` disabled.
fn handle_logs(log_stream: &LogStreamHandle, tokens: &[String]) {
    let sub = tokens.get(1).map(String::as_str).unwrap_or("tail");
    match sub {
        "tail" => {
            let n = tokens
                .get(2)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(LOGS_TAIL_REPL_DEFAULT);
            let events = log_stream.tail(n);
            if events.is_empty() {
                println!("logs: ring buffer empty");
                return;
            }
            for ev in &events {
                println!(
                    "{} {:>5} {}  {}",
                    ev.ts.format("%H:%M:%S%.3f"),
                    ev.level,
                    ev.target,
                    ev.message
                );
            }
        }
        other => {
            eprintln!("logs: unknown subcommand `{other}`; expected `tail [<n>]`");
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

fn queue_clear(
    state: &Arc<Mutex<QueueState>>,
    state_path: &Path,
    _queue_paused: &Arc<AtomicBool>,
    event_stream: &EventStreamHandle,
) {
    let (n, tick) = {
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
        let tick = queue::derive_tick(&guard, None);
        (n, tick)
    };
    event_stream.publish(Event::QueueTick(tick));
    println!("queue clear: cancelled {n} pending job(s)");
}

fn queue_set_paused(
    state: &Arc<Mutex<QueueState>>,
    state_path: &Path,
    queue_paused: &Arc<AtomicBool>,
    event_stream: &EventStreamHandle,
    paused: bool,
) {
    queue_paused.store(paused, Ordering::Release);
    let tick = {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(err) => {
                eprintln!("bookrack: queue state mutex poisoned: {err}");
                return;
            }
        };
        guard.paused = paused;
        if let Err(err) = queue::save_atomic(&guard, state_path) {
            eprintln!("bookrack: persist queue state: {err}");
            return;
        }
        queue::derive_tick(&guard, None)
    };
    event_stream.publish(Event::QueueTick(tick));
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
    println!("  logs tail [<n>]  Print the most recent <n> log events (default 50)");
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
