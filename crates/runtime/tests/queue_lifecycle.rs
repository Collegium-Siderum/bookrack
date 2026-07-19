// SPDX-License-Identifier: Apache-2.0

//! Integration test for the `queue.pause` / `queue.resume` /
//! `queue.clear` control-plane methods introduced in PR-1.
//!
//! Boots a [`DaemonRuntime`] with the queue worker spawned, then
//! drives a single control-socket client through each mutation while a
//! second client observes the `queue.tick` channel. The assertions
//! check the on-disk `paused` flag (mirrored into `queue.list`), the
//! count of `Pending` rows after `clear`, and that every mutation
//! emits a `queue.tick`.
//!
//! Ignored by default because the bring-up calls
//! [`bookrack_query::Library::open`], which probes the configured
//! Ollama daemon for the embedding model's dimension.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Context, Result, eyre};
use serde_json::Value;
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

static DAEMON_STATE_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Redirect the daemon state directory into a per-binary tempdir so
/// bring-up never touches the user's real per-user data directory.
fn isolate_daemon_state_dir() {
    DAEMON_STATE_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("daemon state tempdir");
        // SAFETY: env is mutated exactly once, inside
        // `OnceLock::get_or_init`'s single-initialization guarantee,
        // as the first statement of every test in this binary, before
        // any concurrent env reads.
        unsafe { std::env::set_var("BOOKRACK_DAEMON_STATE_DIR", dir.path()) };
        dir
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn pause_resume_clear_round_trip_through_control_plane() -> Result<()> {
    isolate_daemon_state_dir();
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
        await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(2))
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
        await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(2))
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
        await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(2))
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

    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    Ok(())
}
