// SPDX-License-Identifier: Apache-2.0

//! `bookrack run` — the daemon process entry point.
//!
//! One [`run_daemon`] call brings up the session-scoped process: it
//! acquires the machine-wide TTY lock, opens the
//! [`bookrack_ops::registry::LibraryRegistry`] that every later subsystem
//! routes through, mounts the MCP listener as an in-process task, spawns
//! the queue worker, and joins a coordinated shutdown channel that signal
//! handlers and the control-plane `daemon.shutdown` RPC all write to.
//!
//! The daemon owns no stdin and runs headless. Operators reach it through
//! the control-plane JSON-RPC socket: one-shot subcommands (`bookrack
//! ingest`, `bookrack metadata set`, ...), `bookrack rpc call <method>`
//! for ad-hoc calls, the desktop tray, and the MCP server for agent
//! clients.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_cli::error::BookrackCliError;
use bookrack_config::{LibrarySelection, LogConfig};
use bookrack_core::Explain;
use bookrack_ops::Caller;
use bookrack_runtime::backend_probe::PreflightRefusal;
use bookrack_runtime::control::HealthProbe;
use bookrack_runtime::mcp_endpoint::McpBindRefusal;
use bookrack_runtime::queue::QueueLoadError;
use bookrack_runtime::rerank_supervisor::RerankerModelRefusal;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Context, Result};

/// CLI inputs for [`run_daemon`]. Parsed from the `Run` clap variant.
pub struct RunOpts {
    /// Library selection forwarded to [`Config::resolve`].
    pub selection: LibrarySelection,
    /// Override the MCP listener address; falls back to
    /// [`bookrack_config::McpConfig::from_env`].
    pub mcp_addr: Option<SocketAddr>,
    /// Skip binding the MCP listener. The daemon still acquires the
    /// TTY lock and opens the registry; useful for development and for
    /// running the daemon on a host where another tool already owns
    /// the MCP port.
    pub no_mcp: bool,
    /// Override the runtime directory. Falls back to
    /// `BOOKRACK_RUNTIME_DIR` or the platform default. Primarily a
    /// test hook so suites can isolate the TTY lock from the
    /// operator's session.
    pub runtime_dir: Option<PathBuf>,
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
    };

    let mut runtime = match DaemonRuntime::start(runtime_opts).await {
        Ok(rt) => rt,
        Err(err) => {
            if bookrack_session::is_lock_conflict(&err) {
                return handle_lock_conflict(err, &lock_path).await;
            }
            // A refused pre-flight is operator input, not a bug: hand
            // it to the typed CLI error so it picks up exit 2 and the
            // three-part reporter. `runtime` cannot depend on `cli`,
            // so the conversion has to happen on this side.
            if let Some(refusal) = err.downcast_ref::<PreflightRefusal>() {
                return Err(BookrackCliError::PreflightRefused {
                    problem: refusal.problem.clone(),
                }
                .into());
            }
            // An endpoint the daemon cannot take is the same class of
            // failure: refuse before announcing an address, so no
            // success line goes out for a surface that is not there.
            if let Some(refusal) = err.downcast_ref::<McpBindRefusal>() {
                return Err(BookrackCliError::PreflightRefused {
                    problem: refusal.problem.clone(),
                }
                .into());
            }
            // A reranker model the profile promises and nothing provides
            // is operator input of the same kind: the refusal names the
            // route that does provide it.
            if let Some(refusal) = err.downcast_ref::<RerankerModelRefusal>() {
                return Err(BookrackCliError::PreflightRefused {
                    problem: refusal.explain(),
                }
                .into());
            }
            // A queue document this binary will not read is the third:
            // the state is intact and the version that wrote it still
            // reads it, so the operator has a next step and the failure
            // is not a bug report.
            if let Some(refusal) = err.downcast_ref::<QueueLoadError>() {
                return Err(BookrackCliError::PreflightRefused {
                    problem: refusal.explain(),
                }
                .into());
            }
            return Err(err);
        }
    };

    // The listener socket is bound by now, so `mcp_label` is the
    // address a client can connect to — including the port the kernel
    // assigned when the configuration asked for `port 0`.
    println!(
        "bookrack daemon running: pid={} mcp={} control_sock={}",
        std::process::id(),
        runtime.mcp_label,
        runtime.control_sock.path.display(),
    );
    println!("stop with Ctrl-C or `bookrack quit`");

    let mcp_handle = bookrack_mcp::spawn_listener(&mut runtime);

    // Foreground task: an async future that resolves on the shutdown
    // broadcast. The foreground handle is required by
    // `run_until_shutdown`; a no-op blocking thread would stall tokio
    // teardown after a control-plane `daemon.shutdown`.
    let mut shutdown_rx = runtime.shutdown_tx.subscribe();
    let fg_handle = tokio::spawn(async move {
        let _ = shutdown_rx.recv().await;
        eyre::Ok(())
    });

    runtime.run_until_shutdown(mcp_handle, fg_handle).await
}

/// Resolve a session-lock conflict by probing the running daemon. A
/// healthy daemon prints the recorded pid and control socket and
/// returns `Ok` so a second `bookrack run` invocation is a no-op. A
/// held lock whose recorded daemon does not answer returns
/// [`BookrackCliError::StaleSessionLock`] (exit 3); an unprobeable
/// lock returns [`BookrackCliError::SessionLockUnreadable`] (exit 1).
async fn handle_lock_conflict(err: eyre::Report, lock_path: &Path) -> Result<()> {
    let info = match bookrack_session::peek_lock(lock_path) {
        Ok(Some(i)) => i,
        Ok(None) | Err(_) => {
            return Err(BookrackCliError::SessionLockUnreadable {
                message: format!("{err:#}"),
            }
            .into());
        }
    };
    let probe = bookrack_runtime::control::probe(&info, Duration::from_secs(2)).await;
    match probe {
        HealthProbe::Healthy(pid, sock) => {
            println!(
                "bookrack daemon already running: pid={pid} control_sock={}",
                sock.display()
            );
            Ok(())
        }
        HealthProbe::Stale => Err(BookrackCliError::StaleSessionLock {
            path: lock_path.to_path_buf(),
            pid: info.pid,
        }
        .into()),
        HealthProbe::Unprobeable => Err(BookrackCliError::SessionLockUnreadable {
            message: format!("{err:#}"),
        }
        .into()),
    }
}
