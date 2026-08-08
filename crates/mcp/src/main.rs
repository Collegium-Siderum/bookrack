// SPDX-License-Identifier: Apache-2.0

//! bookrack MCP daemon entry point.
//!
//! Wraps [`bookrack_runtime::DaemonRuntime`] with the headless profile:
//! no queue worker, stderr-mirrored logging, and the MCP-tagged
//! [`bookrack_ops::Caller`]. Serves MCP over streamable HTTP until
//! the shared shutdown broadcast fires (Ctrl-C, the
//! `session.shutdown` MCP tool).

use std::path::PathBuf;

use bookrack_runtime::mcp_endpoint::McpBindRefusal;
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
    // In `main` rather than `run`: `run` is an ordinary function
    // another caller could reach, and reading a file out of the working
    // directory is the binary's decision, not a callable's.
    let _ = bookrack_config::load_dotenv();
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // A refused bring-up is operator input, not a bug: report the
        // three parts stacked and exit 2, the same code and the same
        // layout `bookrack run` gives the same refusal.
        Err(err) if err.downcast_ref::<McpBindRefusal>().is_some() => {
            let refusal = err.downcast_ref::<McpBindRefusal>().expect("just matched");
            eprintln!("bookrack-mcp: {}", refusal.problem.summary);
            if let Some(detail) = &refusal.problem.data.detail {
                eprintln!("  {detail}");
            }
            if let Some(hint) = &refusal.problem.data.hint {
                eprintln!("  hint: {hint}");
            }
            std::process::ExitCode::from(2)
        }
        Err(err) => {
            eprintln!("Error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    let mut runtime_opts = RuntimeOpts::headless(cli.data_dir, cli.library);
    runtime_opts.spawn_queue_worker = cli.with_queue_worker;
    runtime_opts.mcp_tools = bookrack_mcp::list_tools();
    // The runtime resolves and binds the MCP address itself, so the
    // address this binary announces is the one it actually owns.
    let mut runtime = DaemonRuntime::start(runtime_opts).await?;

    let serve_handle = bookrack_mcp::spawn_listener(&mut runtime);

    // Headless profile has no REPL; park a no-op blocking thread so
    // the shared `run_until_shutdown` join contract is satisfied.
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> {
        std::thread::park();
        Ok(())
    });

    runtime.run_until_shutdown(serve_handle, repl_handle).await
}
