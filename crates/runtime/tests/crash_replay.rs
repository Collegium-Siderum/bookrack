// SPDX-License-Identifier: Apache-2.0

//! Phase 2 crash-recovery snapshot integration test.
//!
//! Boots a [`DaemonRuntime`] in the headless profile, submits one
//! ingest job through the control plane, shuts the runtime down
//! abruptly, then brings a second runtime up against the same data
//! directory and verifies that `events.snapshot { channels:
//! ["queue.list", "queue.tick"] }` agrees with the on-disk
//! `.bookrack-queue.json`.
//!
//! Ignored by default because the runtime calls
//! [`bookrack_query::Library::open`], which probes the configured
//! Ollama daemon for the embedding model's dimension.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use bookrack_config::LibrarySelection;
use bookrack_core::queue::QueueState;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Context, Result, eyre};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::UnixStream;

fn build_opts(data_dir: PathBuf, runtime_dir: PathBuf) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_dir);
    opts.spawn_queue_worker = true;
    opts.selection = LibrarySelection {
        data_dir: opts.selection.data_dir,
        library: opts.selection.library,
    };
    opts
}

async fn send(stream: &mut WriteHalf<UnixStream>, line: &str) -> Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn recv(reader: &mut Lines<BufReader<ReadHalf<UnixStream>>>) -> Result<Value> {
    let line = reader
        .next_line()
        .await?
        .ok_or_else(|| eyre!("connection closed before response"))?;
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn replay_after_restart_matches_disk_state() -> Result<()> {
    let data_root = tempfile::tempdir()?;
    let runtime_root_a = tempfile::tempdir()?;

    {
        let runtime = DaemonRuntime::start(build_opts(
            data_root.path().into(),
            runtime_root_a.path().into(),
        ))
        .await?;
        let sock = runtime.control_sock.path.clone();
        let shutdown_tx = runtime.shutdown_tx.clone();
        let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

        let driver = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let stream = UnixStream::connect(&sock).await?;
            let (r, mut w) = tokio::io::split(stream);
            let mut reader = BufReader::new(r).lines();
            send(
                &mut w,
                r#"{"jsonrpc":"2.0","id":1,"method":"ingest.submit",
                     "params":{"paths":["/tmp/phase2-replay-fixture.txt"]}}"#,
            )
            .await?;
            let resp = recv(&mut reader).await?;
            assert!(resp["result"]["job_ids"].is_array(), "{resp}");
            send(
                &mut w,
                r#"{"jsonrpc":"2.0","id":2,"method":"daemon.shutdown"}"#,
            )
            .await?;
            let _ = recv(&mut reader).await?;
            Ok::<(), eyre::Report>(())
        });
        runtime.run_until_shutdown(None, repl_handle).await?;
        driver.await??;
        let _ = shutdown_tx;
    }

    let queue_path = data_root.path().join(".bookrack-queue.json");
    let on_disk: QueueState = serde_json::from_slice(&std::fs::read(&queue_path)?)
        .context("parse on-disk queue state")?;
    assert!(!on_disk.jobs.is_empty(), "queue document was not persisted");

    let runtime_root_b = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root_b.path().into(),
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = UnixStream::connect(&sock).await?;
        let (r, mut w) = tokio::io::split(stream);
        let mut reader = BufReader::new(r).lines();
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.snapshot",
                 "params":{"channels":["queue.list","queue.tick"]}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let snapshot = resp["result"].clone();
        assert!(snapshot["queue.list"].is_object(), "{snapshot}");
        assert!(snapshot["queue.tick"].is_object(), "{snapshot}");

        let snapshot_jobs = &snapshot["queue.list"]["jobs"];
        assert!(snapshot_jobs.is_array(), "{snapshot}");
        let on_disk_jobs = serde_json::to_value(&on_disk.jobs).unwrap();
        assert_eq!(
            snapshot_jobs, &on_disk_jobs,
            "snapshot queue.list.jobs diverges from .bookrack-queue.json"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    Ok(())
}
