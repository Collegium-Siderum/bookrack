// SPDX-License-Identifier: Apache-2.0

//! bookrack MCP daemon entry point.
//!
//! Wraps [`bookrack_runtime::DaemonRuntime`] with the headless profile:
//! no queue worker, stderr-mirrored logging, and the MCP-tagged
//! [`bookrack_ops::Caller`]. Serves MCP over streamable HTTP until
//! the shared shutdown broadcast fires (Ctrl-C, the
//! `session.shutdown` MCP tool).

use std::path::PathBuf;

use bookrack_config::McpConfig;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::Result;

#[derive(clap::Parser)]
#[command(
    name = "bookrack-mcp",
    version,
    about = "Serve a local library over MCP."
)]
struct Cli {
    /// Serve the library at this data root, overriding the environment.
    /// Mutually exclusive with `--library`.
    #[arg(long, conflicts_with = "library")]
    data_dir: Option<PathBuf>,
    /// Serve the named library from the registry (see BOOKRACK_REGISTRY).
    /// Mutually exclusive with `--data-dir`.
    #[arg(long)]
    library: Option<String>,
    /// Spawn the persistent ingest queue worker. Off by default so a
    /// server-class headless entry does not start work the operator
    /// did not ask for; on, the control-plane `ingest.submit` /
    /// `vectors.*` / `corpus.rebuild` family of methods become live.
    /// With the flag off, those methods return JSON-RPC error
    /// `-32002 queue worker disabled in headless mode`.
    #[arg(long)]
    with_queue_worker: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    let mcp_cfg = McpConfig::from_env();
    let mut runtime_opts = RuntimeOpts::headless(cli.data_dir, cli.library);
    runtime_opts.spawn_queue_worker = cli.with_queue_worker;
    runtime_opts.mcp_tools = bookrack_mcp::list_tools();
    let runtime = DaemonRuntime::start(runtime_opts).await?;

    let shutdown_tx = runtime.shutdown_tx.clone();
    let shutdown_rx = shutdown_tx.subscribe();
    let registry = runtime.registry.clone();
    let info_context = runtime.info_context.clone();
    let log_stream = runtime.log_stream.clone();
    let queue_state = runtime.queue_state.clone();
    let started_at = runtime.started_at;
    let addr = mcp_cfg.addr.clone();

    let serve_handle = tokio::spawn(async move {
        bookrack_mcp::serve(
            registry,
            info_context,
            started_at,
            log_stream,
            queue_state,
            shutdown_tx,
            &addr,
            shutdown_rx,
        )
        .await
    });

    // Headless profile has no REPL; park a no-op blocking thread so
    // the shared `run_until_shutdown` join contract is satisfied.
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> {
        std::thread::park();
        Ok(())
    });

    runtime
        .run_until_shutdown(Some(serve_handle), repl_handle)
        .await
}
