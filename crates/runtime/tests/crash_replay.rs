// SPDX-License-Identifier: Apache-2.0

//! Phase 2 queue-persistence replay integration test.
//!
//! Boots a [`DaemonRuntime`] in the headless profile, pauses the
//! queue, submits one ingest job through the control plane, shuts the
//! runtime down, then brings a second runtime up against the same
//! daemon state directory and verifies that `events.snapshot
//! { channels: ["queue.list", "queue.tick"] }` agrees with the
//! on-disk queue snapshot. The pause keeps the worker in both
//! runtimes away from the job, so the disk document and the snapshot
//! are compared over a stable state.
//!
//! The embedder probe daemon bring-up performs is answered by the
//! loopback stub in `common`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use bookrack_core::queue::QueueState;
use eyre::{Context, Result};
use serde_json::Value;

use crate::common::{build_opts, connect, init_test_env, join_with_deadline, recv, send};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_after_restart_matches_disk_state() -> Result<()> {
    init_test_env();
    let state_dir = std::path::PathBuf::from(std::env::var("BOOKRACK_DAEMON_STATE_DIR")?);
    let data_root = tempfile::tempdir()?;
    let runtime_root_a = tempfile::tempdir()?;

    {
        let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
            data_root.path().into(),
            runtime_root_a.path().into(),
            true,
        ))
        .await?;
        let sock = runtime.control_sock.path.clone();
        let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

        let driver = tokio::spawn(async move {
            let (mut reader, mut w) = connect(&sock).await?;
            // Pause before submitting so the worker never moves the
            // job out of `Pending`; the persisted document is then
            // deterministic.
            send(&mut w, r#"{"jsonrpc":"2.0","id":1,"method":"queue.pause"}"#).await?;
            let resp = recv(&mut reader).await?;
            assert_eq!(resp["result"]["paused"], Value::Bool(true), "{resp}");
            send(
                &mut w,
                r#"{"jsonrpc":"2.0","id":2,"method":"ingest.submit","params":{"paths":["/tmp/phase2-replay-fixture.txt"]}}"#,
            )
            .await?;
            let resp = recv(&mut reader).await?;
            assert!(resp["result"]["job_ids"].is_array(), "{resp}");
            send(
                &mut w,
                r#"{"jsonrpc":"2.0","id":3,"method":"daemon.shutdown"}"#,
            )
            .await?;
            let _ = recv(&mut reader).await?;
            Ok::<(), eyre::Report>(())
        });
        join_with_deadline(runtime, repl_handle, driver).await?;
    }

    let queue_path = state_dir.join("queue.json");
    let on_disk: QueueState = serde_json::from_slice(&std::fs::read(&queue_path)?)
        .context("parse on-disk queue state")?;
    assert!(!on_disk.jobs.is_empty(), "queue document was not persisted");

    let runtime_root_b = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root_b.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.snapshot","params":{"channels":["queue.list","queue.tick"]}}"#,
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
            "snapshot queue.list.jobs diverges from the on-disk queue snapshot"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}
