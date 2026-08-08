// SPDX-License-Identifier: Apache-2.0

//! The MCP endpoint is a bring-up resource, not a task's private
//! detail.
//!
//! Two halves of one contract. An address the daemon cannot take
//! refuses the whole bring-up, before anything reports success —
//! previously the bind happened inside the listener task, so `run`,
//! `status`, and `doctor` all announced an address that belonged to
//! another process. And the address the daemon announces is the one
//! the socket actually holds, which is the only way `port 0` can be
//! useful: the kernel picks the port, and every reader has to learn
//! which one it picked.
//!
//! The embedder probe bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

mod common;

use std::path::PathBuf;

use bookrack_runtime::mcp_endpoint::McpBindRefusal;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::Result;
use tokio::net::{TcpListener, TcpStream};

/// Headless bring-up options that serve MCP at `mcp_addr`.
fn opts_serving_mcp(data_dir: PathBuf, runtime_dir: PathBuf, mcp_addr: &str) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = false;
    opts.runtime_dir = Some(runtime_dir);
    opts.mcp_addr = Some(mcp_addr.parse().expect("test address parses"));
    opts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_occupied_address_refuses_bring_up() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;

    let occupant = TcpListener::bind("127.0.0.1:0").await?;
    let addr = occupant.local_addr()?.to_string();

    let err = DaemonRuntime::start(opts_serving_mcp(
        data_root.path().into(),
        runtime_root.path().into(),
        &addr,
    ))
    .await
    .err()
    .expect("bring-up must refuse an address it cannot bind");

    let refusal = err
        .downcast_ref::<McpBindRefusal>()
        .unwrap_or_else(|| panic!("expected a typed refusal, got: {err:#}"));
    assert!(
        refusal.problem.summary.contains(&addr),
        "the operator must be told which address failed: {}",
        refusal.problem.summary
    );

    // The refusal releases what it took: the session lock is free for
    // the next attempt, so a corrected retry is not blocked by the
    // failed one.
    let lock_path = runtime_root.path().join(bookrack_session::tty_lock_name());
    assert!(
        !bookrack_session::lock_is_held(&lock_path)?,
        "a refused bring-up must not leave the session lock held"
    );

    drop(occupant);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn port_zero_reports_the_address_the_socket_holds() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;

    let runtime = DaemonRuntime::start(opts_serving_mcp(
        data_root.path().into(),
        runtime_root.path().into(),
        "127.0.0.1:0",
    ))
    .await?;
    let shutdown_tx = runtime.shutdown_tx.clone();
    let label = runtime.mcp_label.clone();
    let lock_path = runtime.lock_path.clone();

    let bound: std::net::SocketAddr = label
        .parse()
        .unwrap_or_else(|e| panic!("reported address {label:?} is not an address: {e}"));
    assert_ne!(
        bound.port(),
        0,
        "the reported address must be the assigned port, not the requested one"
    );

    // The socket is open before the listener task exists, so a client
    // can already connect at the instant the daemon announces itself.
    TcpStream::connect(bound)
        .await
        .expect("the announced address must be connectable at bring-up");

    // Every reader of the session lock — `bookrack status`, the
    // second-launch path, an operator's `cat` — gets the same address.
    let info = bookrack_session::peek_lock(&lock_path)?.expect("session lock is readable");
    assert_eq!(
        info.mcp, label,
        "the session lock must record the bound address, not the requested one"
    );

    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let _ = shutdown_tx.send(());
    runtime.run_until_shutdown(None, repl_handle).await?;
    Ok(())
}

/// `--no-mcp` keeps its meaning: nothing is bound, nothing is
/// announced, and bring-up does not depend on any address being free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_mcp_binds_nothing() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;

    let runtime = DaemonRuntime::start(common::build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        false,
    ))
    .await?;
    let shutdown_tx = runtime.shutdown_tx.clone();

    assert_eq!(runtime.mcp_label, "disabled");
    assert!(
        runtime.mcp_listener.is_none(),
        "a session without MCP must hold no listening socket"
    );

    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let _ = shutdown_tx.send(());
    runtime.run_until_shutdown(None, repl_handle).await?;
    Ok(())
}
