// SPDX-License-Identifier: Apache-2.0

//! Phase 2 control-plane write-surface integration test.
//!
//! Boots a [`DaemonRuntime`] in the headless profile, drives one
//! client through the `ingest.submit` → `queue.tick` event path while
//! a second client observes the broadcast over `events.subscribe`,
//! then has a third client race a `vectors.drop` against itself to
//! exercise the `-32001 busy` error code.
//!
//! The embedder probe daemon bring-up performs is answered by the
//! loopback stub in `common`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::time::Duration;

use eyre::{Context, Result};
use serde_json::Value;

use crate::common::{
    await_channel, build_opts, connect, init_test_env, join_with_deadline, recv, send,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_submit_broadcasts_queue_tick_to_subscribers() -> Result<()> {
    init_test_env();
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut obs_reader, mut obs_w) = connect(&sock).await?;
        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#,
        )
        .await?;
        let resp = recv(&mut obs_reader).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");
        // Drain the snapshot bundle so the queue.tick awaited below is
        // the submit broadcast, not the pre-submit snapshot.
        for _ in 0..7 {
            let _ = recv(&mut obs_reader).await?;
        }

        let (mut wr_reader, mut wr_w) = connect(&sock).await?;
        // Pause the worker so the submitted job stays pending: tick
        // values are derived from live queue state at emission time,
        // and a running worker could fail the missing-fixture job
        // before the submit tick is built.
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":2,"method":"queue.pause"}"#,
        )
        .await?;
        let pause = recv(&mut wr_reader).await?;
        assert_eq!(pause["result"]["paused"], Value::Bool(true), "{pause}");

        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":3,"method":"ingest.submit","params":{"paths":["/tmp/phase2-fixture.txt"]}}"#,
        )
        .await?;
        let submit = recv(&mut wr_reader).await?;
        assert!(submit["result"]["job_ids"].is_array(), "{submit}");

        // Tick values are derived from live queue state at emission
        // time and idle ticks may interleave, so await the first tick
        // that reflects the submitted job rather than a fixed ordinal.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let tick = await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(5))
                .await
                .context("expect queue.tick on observer")?;
            if tick["params"]["value"]["pending"].as_u64().unwrap_or(0) >= 1 {
                break;
            }
            eyre::ensure!(
                tokio::time::Instant::now() < deadline,
                "no queue.tick with pending >= 1 arrived: last {tick}"
            );
        }

        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut obs_reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_write_returns_busy_error() -> Result<()> {
    init_test_env();
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut fr_reader, mut fr_w) = connect(&sock).await?;
        let (mut sr_reader, mut sr_w) = connect(&sock).await?;

        // Kick off two writes back-to-back. `vectors.drop` is the
        // simplest write command in the surface — it opens the corpus
        // and drops the ANN index. Whichever lands first holds the
        // write mutex; the other must see `-32001 busy`.
        send(
            &mut fr_w,
            r#"{"jsonrpc":"2.0","id":10,"method":"vectors.drop","params":{"yes":true}}"#,
        )
        .await?;
        send(
            &mut sr_w,
            r#"{"jsonrpc":"2.0","id":11,"method":"vectors.drop","params":{"yes":true}}"#,
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
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
