// SPDX-License-Identifier: Apache-2.0

//! Phase 2 control-plane write-surface integration test.
//!
//! Boots a [`DaemonRuntime`] in the headless profile, drives one
//! client through the `ingest.submit` → `queue.tick` event path while
//! a second client observes the broadcast over `events.subscribe`,
//! then has a third client race a `vectors.drop` against itself to
//! exercise the `-32001 busy` error code.
//!
//! Ignored by default because the runtime calls
//! [`bookrack_query::Library::open`], which probes the configured
//! Ollama daemon for the embedding model's dimension. Run manually
//! with `cargo test -p bookrack-runtime -- --ignored`.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Context, Result, eyre};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
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

async fn await_channel(
    reader: &mut Lines<BufReader<ReadHalf<UnixStream>>>,
    channel: &str,
    timeout: Duration,
) -> Result<Value> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Err(eyre!("timed out waiting for channel {channel}")),
            frame = reader.next_line() => {
                let line = frame?.ok_or_else(|| eyre!("eof while awaiting {channel}"))?;
                let v: Value = serde_json::from_str(&line)?;
                if v.get("method").is_some()
                    && v["params"]["channel"].as_str() == Some(channel)
                {
                    return Ok(v);
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn ingest_submit_broadcasts_queue_tick_to_subscribers() -> Result<()> {
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

        let observer = UnixStream::connect(&sock).await?;
        let (obs_r, mut obs_w) = tokio::io::split(observer);
        let mut obs_reader = BufReader::new(obs_r).lines();
        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#,
        )
        .await?;
        let resp = recv(&mut obs_reader).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");

        let writer = UnixStream::connect(&sock).await?;
        let (wr_r, mut wr_w) = tokio::io::split(writer);
        let mut wr_reader = BufReader::new(wr_r).lines();
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":2,"method":"ingest.submit",
                 "params":{"paths":["/tmp/phase2-fixture.txt"]}}"#,
        )
        .await?;
        let submit = recv(&mut wr_reader).await?;
        assert!(submit["result"]["job_ids"].is_array(), "{submit}");

        let tick = await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(2))
            .await
            .context("expect queue.tick on observer")?;
        assert!(
            tick["params"]["value"]["pending"].as_u64().unwrap_or(0) >= 1,
            "{tick}"
        );

        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut obs_reader).await?;
        Ok::<(), eyre::Report>(())
    });

    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn second_write_returns_busy_error() -> Result<()> {
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
        let first = UnixStream::connect(&sock).await?;
        let (fr_r, mut fr_w) = tokio::io::split(first);
        let mut fr_reader = BufReader::new(fr_r).lines();
        let second = UnixStream::connect(&sock).await?;
        let (sr_r, mut sr_w) = tokio::io::split(second);
        let mut sr_reader = BufReader::new(sr_r).lines();

        // Kick off two writes back-to-back. `vectors.drop` is the
        // simplest write command in the surface — it opens the corpus
        // and drops the ANN index. Whichever lands first holds the
        // write mutex; the other must see `-32001 busy`.
        send(
            &mut fr_w,
            r#"{"jsonrpc":"2.0","id":10,"method":"vectors.drop"}"#,
        )
        .await?;
        send(
            &mut sr_w,
            r#"{"jsonrpc":"2.0","id":11,"method":"vectors.drop"}"#,
        )
        .await?;
        let resp_a = recv(&mut fr_reader).await?;
        let resp_b = recv(&mut sr_reader).await?;

        let codes: Vec<Option<i64>> = [&resp_a, &resp_b]
            .iter()
            .map(|r| r["error"]["code"].as_i64())
            .collect();
        assert!(
            codes.contains(&Some(-32001_i64)),
            "expected one response with code -32001, got {codes:?} payloads {resp_a} / {resp_b}"
        );

        send(
            &mut fr_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut fr_reader).await?;
        let _ = (resp_a, resp_b, json!(null));
        Ok::<(), eyre::Report>(())
    });

    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    Ok(())
}
