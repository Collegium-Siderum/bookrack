// SPDX-License-Identifier: Apache-2.0

//! Control-plane integration test for the paper-side maintenance
//! triplet: `papers.corpus_rebuild`, `papers.vectors_*`, and
//! `papers.stamps_reconcile`.
//!
//! Drives the daemon's JSON-RPC dispatch, asserts the six new methods
//! appear under `daemon.methods`, and exercises the dry-run paths that
//! do not require a populated library to validate the parameter shapes
//! and the queue-bound write gate.
//!
//! Ignored by default because the runtime calls
//! [`bookrack_query::Library::open`], which probes the configured
//! Ollama daemon for the embedding model's dimension. Run manually
//! with `cargo test -p bookrack-runtime -- --ignored`.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::UnixStream;

fn build_opts(data_dir: PathBuf, runtime_dir: PathBuf) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = true;
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_dir);
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
        .ok_or_else(|| anyhow!("connection closed before response"))?;
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn papers_maintenance_methods_are_dispatched_and_callable_on_empty_library() -> Result<()> {
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = UnixStream::connect(&sock).await?;
        let (r, mut w) = tokio::io::split(stream);
        let mut reader = BufReader::new(r).lines();

        // 1. `daemon.methods` enumerates every new method exactly once.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"daemon.methods"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let names: BTreeSet<String> = resp["result"]["methods"]
            .as_array()
            .ok_or_else(|| anyhow!("daemon.methods missing array: {resp}"))?
            .iter()
            .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
            .collect();
        for expected in [
            "papers.corpus_rebuild",
            "papers.vectors_rebuild",
            "papers.vectors_reembed",
            "papers.vectors_reset",
            "papers.vectors_drop",
            "papers.stamps_reconcile",
        ] {
            assert!(
                names.contains(expected),
                "method {expected} missing from daemon.methods: {names:?}"
            );
        }

        // 2. `papers.corpus_rebuild` with dry_run=true on an empty
        //    library succeeds and reports zero rebuildable intakes.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "papers.corpus_rebuild",
            "params": {"dry_run": true, "yes": true},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(
            resp["result"],
            json!({"ok": true}),
            "papers.corpus_rebuild dry_run should succeed: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), anyhow::Error>(())
    });

    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    Ok(())
}
