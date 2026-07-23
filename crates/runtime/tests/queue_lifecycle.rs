// SPDX-License-Identifier: Apache-2.0

//! Integration test for the `queue.pause` / `queue.resume` /
//! `queue.clear` control-plane methods.
//!
//! Boots a [`DaemonRuntime`] with the queue worker spawned, then
//! drives a single control-socket client through each mutation while a
//! second client observes the `queue.tick` channel. The assertions
//! check the on-disk `paused` flag (mirrored into `queue.list`), the
//! count of `Pending` rows after `clear`, and that every mutation
//! emits a `queue.tick`.
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
async fn pause_resume_clear_round_trip_through_control_plane() -> Result<()> {
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
        // Drain the snapshot bundle so the ticks awaited below are the
        // mutation broadcasts, not the pre-mutation snapshot.
        for _ in 0..7 {
            let _ = recv(&mut obs_reader).await?;
        }

        let (mut wr_reader, mut wr_w) = connect(&sock).await?;

        // queue.pause toggles paused=true.
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":10,"method":"queue.pause"}"#,
        )
        .await?;
        let pause_resp = recv(&mut wr_reader).await?;
        assert_eq!(
            pause_resp["result"]["paused"],
            Value::Bool(true),
            "{pause_resp}"
        );
        await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(5))
            .await
            .context("queue.tick after queue.pause")?;

        // Submit two ingest jobs. With the worker paused, both stay
        // pending so queue.clear has rows to trim.
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":11,"method":"ingest.submit","params":{"paths":["/tmp/queue-lifecycle-a.epub"]}}"#,
        )
        .await?;
        let _ = recv(&mut wr_reader).await?;
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":12,"method":"ingest.submit","params":{"paths":["/tmp/queue-lifecycle-b.epub"]}}"#,
        )
        .await?;
        let _ = recv(&mut wr_reader).await?;

        // queue.list reflects paused=true and shows the pending rows.
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":13,"method":"queue.list"}"#,
        )
        .await?;
        let list_resp = recv(&mut wr_reader).await?;
        assert_eq!(
            list_resp["result"]["paused"],
            Value::Bool(true),
            "{list_resp}"
        );
        assert!(
            list_resp["result"]["jobs"]
                .as_array()
                .map(|j| j.iter().filter(|r| r["state"] == "pending").count() >= 2)
                .unwrap_or(false),
            "{list_resp}"
        );

        // queue.clear trims pending rows.
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":14,"method":"queue.clear"}"#,
        )
        .await?;
        let clear_resp = recv(&mut wr_reader).await?;
        assert!(
            clear_resp["result"]["cleared"].as_u64().unwrap_or(0) >= 2,
            "{clear_resp}"
        );
        assert_eq!(
            clear_resp["result"]["paused"],
            Value::Bool(true),
            "{clear_resp}"
        );
        await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(5))
            .await
            .context("queue.tick after queue.clear")?;

        // queue.resume toggles paused back to false.
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":15,"method":"queue.resume"}"#,
        )
        .await?;
        let resume_resp = recv(&mut wr_reader).await?;
        assert_eq!(
            resume_resp["result"]["paused"],
            Value::Bool(false),
            "{resume_resp}"
        );
        await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(5))
            .await
            .context("queue.tick after queue.resume")?;

        // After resume, queue.list reports paused=false and no pending
        // rows (the trim happened while paused).
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":16,"method":"queue.list"}"#,
        )
        .await?;
        let list_after = recv(&mut wr_reader).await?;
        assert_eq!(
            list_after["result"]["paused"],
            Value::Bool(false),
            "{list_after}"
        );
        let pending_after = list_after["result"]["jobs"]
            .as_array()
            .map(|j| j.iter().filter(|r| r["state"] == "pending").count())
            .unwrap_or(usize::MAX);
        assert_eq!(pending_after, 0, "{list_after}");

        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut wr_reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
