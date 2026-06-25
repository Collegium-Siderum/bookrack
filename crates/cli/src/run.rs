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
//! ingest`, `bookrack metadata set`, ...), `bookrack exec <method>` for
//! ad-hoc calls, the desktop tray, and the MCP server for agent clients.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use bookrack_config::{LibrarySelection, LogConfig};
use bookrack_ops::Caller;
use bookrack_runtime::control::HealthProbe;
use bookrack_runtime::{DaemonRuntime, LaunchMode, RuntimeOpts};
use serde_json::Value;

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
    println!("stop with Ctrl-C or `bookrack quit`");

    let mcp_handle = bookrack_mcp::spawn_listener(&runtime);

    // Foreground task: an async future that resolves on the shutdown
    // broadcast. Mirrors the headless pattern used by the Tauri shell
    // (`crates/app`). The foreground handle is required by
    // `run_until_shutdown`; a no-op blocking thread would stall tokio
    // teardown after a control-plane `daemon.shutdown`.
    let mut shutdown_rx = runtime.shutdown_tx.subscribe();
    let fg_handle = tokio::spawn(async move {
        let _ = shutdown_rx.recv().await;
        anyhow::Ok(())
    });

    runtime.run_until_shutdown(mcp_handle, fg_handle).await
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
