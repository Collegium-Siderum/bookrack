// SPDX-License-Identifier: Apache-2.0

//! Queue persistence and crash-path integration tests.
//!
//! Three restart shapes against one daemon state directory:
//!
//! * `replay_after_restart_matches_disk_state` — orderly path: pause,
//!   submit, `daemon.shutdown`, then a second runtime whose
//!   `events.snapshot` must agree with the on-disk queue document.
//! * `abrupt_termination_preserves_submitted_jobs_for_replay` — crash
//!   path: the first runtime is dropped without any shutdown RPC and
//!   without ever entering `run_until_shutdown`, pinning that
//!   `ingest.submit` persists the document when it answers rather
//!   than at shutdown, and that a restarted runtime serves the dead
//!   session's jobs.
//! * `crash_recovery_resets_a_running_job_to_pending_on_restart` —
//!   recovery path: the state directory is seeded with the document a
//!   crash mid-job leaves behind (one `Running` row), and the
//!   restarted worker must reset it to `Pending`, persist the reset,
//!   and serve it over `events.snapshot`.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use std::time::Duration;

use bookrack_core::ItemKind;
use bookrack_core::queue::{JobState, Priority, QUEUE_SCHEMA_VERSION, QueueJob, QueueState};
use eyre::{Context, Result, bail};
use serde_json::Value;

use crate::common::{build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_after_restart_matches_disk_state() -> Result<()> {
    process_env(ProcessEnv::daemon());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abrupt_termination_preserves_submitted_jobs_for_replay() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let state_dir = std::path::PathBuf::from(std::env::var("BOOKRACK_DAEMON_STATE_DIR")?);
    let data_root = tempfile::tempdir()?;
    let runtime_root_a = tempfile::tempdir()?;

    // First runtime: pause, submit one job, then die abruptly. The
    // runtime is dropped without `daemon.shutdown` and without
    // entering `run_until_shutdown`, so no orderly drain ever runs;
    // whatever is on disk afterwards is what the write handlers
    // persisted before they answered. The pause keeps the worker
    // (required by `ingest.submit`) away from the job so it sits in
    // `Pending` on both sides of the restart.
    {
        let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
            data_root.path().into(),
            runtime_root_a.path().into(),
            true,
        ))
        .await?;
        let (mut reader, mut w) = connect(&runtime.control_sock.path).await?;
        send(&mut w, r#"{"jsonrpc":"2.0","id":1,"method":"queue.pause"}"#).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["result"]["paused"], Value::Bool(true), "{resp}");
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"ingest.submit","params":{"paths":["/tmp/abrupt-replay-fixture.txt"]}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert!(resp["result"]["job_ids"].is_array(), "{resp}");
        drop(runtime);
    }

    let queue_path = state_dir.join("queue.json");
    let on_disk: QueueState = serde_json::from_slice(&std::fs::read(&queue_path)?)
        .context("parse on-disk queue state")?;
    assert_eq!(
        on_disk.jobs.len(),
        1,
        "queue document was not persisted at submit time"
    );
    assert_eq!(on_disk.jobs[0].state, JobState::Pending);

    let runtime_root_b = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root_b.path().into(),
        false,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.snapshot","params":{"channels":["queue.list"]}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let snapshot_jobs = &resp["result"]["queue.list"]["jobs"];
        assert!(snapshot_jobs.is_array(), "{resp}");
        let on_disk_jobs = serde_json::to_value(&on_disk.jobs).unwrap();
        assert_eq!(
            snapshot_jobs, &on_disk_jobs,
            "snapshot queue.list.jobs diverges from the document the dead session left behind"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_recovery_resets_a_running_job_to_pending_on_restart() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let state_dir = std::path::PathBuf::from(std::env::var("BOOKRACK_DAEMON_STATE_DIR")?);
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;

    // Seed the document a crash mid-job leaves behind: one row the
    // dead session's worker had pulled to `Running`. The document is
    // paused so the restarted worker recovers the row without
    // immediately re-pulling it, which keeps the observed state
    // stable; a pause taken while a job is running is a reachable
    // pre-crash state, since pausing only stops further pulls.
    let queued_at =
        chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")?.with_timezone(&chrono::Utc);
    let seeded = QueueState {
        schema_version: QUEUE_SCHEMA_VERSION,
        paused: true,
        jobs: vec![QueueJob {
            id: "01890000-0000-7000-8000-000000000001".into(),
            library: String::new(),
            path: "/tmp/crash-recovery-fixture.txt".into(),
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            kind: ItemKind::Book,
            intake_ocr: None,
            audit_profile: None,
            state: JobState::Running,
            queued_at,
            started_at: Some(queued_at),
            finished_at: None,
            error: None,
            merged_into: None,
        }],
    };
    let queue_path = state_dir.join("queue.json");
    std::fs::write(&queue_path, serde_json::to_vec_pretty(&seeded)?)?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        // The worker's startup recovery runs concurrently with this
        // driver; poll the snapshot until the reset lands. The worker
        // persists the reset before releasing the state lock, so a
        // snapshot that observes `pending` proves the disk write has
        // already happened.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let recovered = loop {
            send(
                &mut w,
                r#"{"jsonrpc":"2.0","id":1,"method":"events.snapshot","params":{"channels":["queue.list"]}}"#,
            )
            .await?;
            let resp = recv(&mut reader).await?;
            let list = resp["result"]["queue.list"].clone();
            if list["jobs"][0]["state"] == "pending" {
                break list;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("running job was never reset to pending: {resp}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(recovered["paused"], Value::Bool(true), "{recovered}");
        assert_eq!(
            recovered["jobs"][0]["started_at"],
            Value::Null,
            "{recovered}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await?;

    // The reset must be durable, so a session that crashes in turn
    // still restarts from `Pending`.
    let on_disk: QueueState = serde_json::from_slice(&std::fs::read(&queue_path)?)
        .context("parse on-disk queue state")?;
    assert!(on_disk.paused);
    assert_eq!(on_disk.jobs[0].state, JobState::Pending);
    assert!(on_disk.jobs[0].started_at.is_none());
    Ok(())
}
