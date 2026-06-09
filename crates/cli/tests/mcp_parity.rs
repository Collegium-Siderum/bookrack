// SPDX-License-Identifier: Apache-2.0

//! Phase 5 control-plane parity: `bookrack run` and `bookrack-mcp`
//! must expose the same method set, and the headless `bookrack-mcp`
//! profile must short-circuit queue-bound writes with a stable
//! `-32002 queue worker disabled in headless mode` JSON-RPC error
//! when invoked without `--with-queue-worker`.
//!
//! Ignored by default because `DaemonRuntime::start` opens an embedding
//! client; without a reachable Ollama daemon the bring-up never
//! finishes.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use serde_json::Value;

fn build_opts_with_queue_worker(
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    spawn_queue_worker: bool,
) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_dir);
    opts.spawn_queue_worker = spawn_queue_worker;
    opts.mcp_tools = bookrack_mcp::list_tools();
    opts.selection = LibrarySelection {
        data_dir: opts.selection.data_dir,
        library: opts.selection.library,
    };
    opts
}

async fn collect_method_names(sock: &std::path::Path) -> Result<BTreeSet<String>> {
    let socket = bookrack_control_client::ControlSocket::from_path(sock.to_path_buf());
    let client = bookrack_control_client::connect(&socket).await?;
    let methods: Value = client.call_raw("daemon.methods", Value::Null).await?;
    let mut names = BTreeSet::new();
    if let Some(rows) = methods.get("methods").and_then(Value::as_array) {
        for row in rows {
            if let Some(name) = row.get("name").and_then(Value::as_str) {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn bookrack_run_and_bookrack_mcp_share_method_set() -> Result<()> {
    let data_root_a = tempfile::tempdir()?;
    let runtime_root_a = tempfile::tempdir()?;
    let data_root_b = tempfile::tempdir()?;
    let runtime_root_b = tempfile::tempdir()?;

    let daemon_a = DaemonRuntime::start(build_opts_with_queue_worker(
        data_root_a.path().into(),
        runtime_root_a.path().into(),
        true,
    ))
    .await?;
    let daemon_b = DaemonRuntime::start(build_opts_with_queue_worker(
        data_root_b.path().into(),
        runtime_root_b.path().into(),
        false,
    ))
    .await?;

    let sock_a = daemon_a.control_sock.path.clone();
    let sock_b = daemon_b.control_sock.path.clone();
    let shutdown_a = daemon_a.shutdown_tx.clone();
    let shutdown_b = daemon_b.shutdown_tx.clone();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let names_a = collect_method_names(&sock_a).await?;
    let names_b = collect_method_names(&sock_b).await?;
    assert_eq!(names_a, names_b, "method sets must match across entries");

    // Headless without `--with-queue-worker` short-circuits queue-bound
    // methods to -32002. `daemon_b` is the headless analogue.
    let socket_b = bookrack_control_client::ControlSocket::from_path(sock_b.clone());
    let client_b = bookrack_control_client::connect(&socket_b).await?;
    let err = client_b
        .call_raw(
            "ingest.submit",
            serde_json::json!({"paths": ["/tmp/x.txt"]}),
        )
        .await
        .expect_err("ingest.submit must short-circuit without queue worker");
    let msg = format!("{err}");
    assert!(
        msg.contains("-32002") || msg.contains("queue worker disabled"),
        "unexpected error: {msg}"
    );

    let _ = shutdown_a.send(());
    let _ = shutdown_b.send(());
    let repl_a = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let repl_b = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    daemon_a.run_until_shutdown(None, repl_a).await?;
    daemon_b.run_until_shutdown(None, repl_b).await?;
    Ok(())
}
