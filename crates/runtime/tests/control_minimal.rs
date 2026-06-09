// SPDX-License-Identifier: Apache-2.0

//! End-to-end Phase 1 control-plane sanity check.
//!
//! Boots a [`DaemonRuntime`] in the headless profile against a fresh
//! tempdir, connects to the bound control socket, and drives the full
//! handshake the contract guarantees:
//!
//! 1. `daemon.version` returns the workspace version.
//! 2. `events.subscribe` returns `{ subscribed: true }` and then a
//!    snapshot bundle of four channels.
//! 3. `doctor.gather` returns a structured report.
//! 4. `daemon.shutdown` triggers a `daemon.state = stopping` notification.
//!
//! Ignored by default because the runtime calls
//! [`bookrack_query::Library::open`], which probes the configured
//! Ollama daemon for the embedding model's dimension.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn build_opts(data_dir: PathBuf, runtime_dir: PathBuf) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_dir);
    opts.selection = LibrarySelection {
        data_dir: opts.selection.data_dir,
        library: opts.selection.library,
    };
    opts
}

async fn send(stream: &mut tokio::io::WriteHalf<UnixStream>, line: &str) -> Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn recv_line(
    reader: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<UnixStream>>>,
) -> Result<Value> {
    let line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("eof while expecting response"))?;
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn full_loop_subscribe_doctor_shutdown() -> Result<()> {
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let shutdown_tx = runtime.shutdown_tx.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = {
        let sock = sock.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let stream = UnixStream::connect(&sock).await?;
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half).lines();

            send(
                &mut write_half,
                r#"{"jsonrpc":"2.0","id":1,"method":"daemon.version"}"#,
            )
            .await?;
            let v = recv_line(&mut reader).await?;
            assert!(v["result"]["version"].as_str().is_some(), "{v}");

            send(
                &mut write_half,
                r#"{"jsonrpc":"2.0","id":2,"method":"events.subscribe"}"#,
            )
            .await?;
            let resp = recv_line(&mut reader).await?;
            assert_eq!(resp["result"]["subscribed"], Value::from(true), "{resp}");

            let mut channels = Vec::new();
            for _ in 0..4 {
                let notif = recv_line(&mut reader).await?;
                channels.push(
                    notif["params"]["channel"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            assert!(channels.iter().any(|c| c == "daemon.state"), "{channels:?}");
            assert!(
                channels.iter().any(|c| c == "daemon.version"),
                "{channels:?}"
            );

            send(
                &mut write_half,
                r#"{"jsonrpc":"2.0","id":3,"method":"doctor.gather"}"#,
            )
            .await?;
            let v = recv_line(&mut reader).await?;
            assert!(v["result"]["rows"].is_array(), "{v}");

            send(
                &mut write_half,
                r#"{"jsonrpc":"2.0","id":4,"method":"daemon.shutdown"}"#,
            )
            .await?;
            let _ = recv_line(&mut reader).await?;

            let stopping = recv_line(&mut reader).await?;
            assert_eq!(stopping["params"]["channel"], "daemon.state");
            assert_eq!(stopping["params"]["value"], "stopping");
            Ok::<(), anyhow::Error>(())
        })
    };

    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    let _ = shutdown_tx; // keep the broadcast alive in scope
    Ok(())
}
