// SPDX-License-Identifier: Apache-2.0

//! The build-machine prefix of a dependency source location does not
//! leave the daemon, and a book's text is never touched.
//!
//! A location reaches the daemon's surfaces three ways besides an error
//! bookrack wraps itself: a dependency logging an error's text straight
//! into `tracing`, a failure recorded in the queue document before
//! redaction existed, and a book that quotes such a path in its prose.
//! The first two come out redacted — through `logs.tail` and the `log`
//! event, and through `queue.list`, the subscription snapshot, and the
//! queue document the worker saves on start. The third comes out of a
//! search exactly as it went in.

#![cfg(unix)]

mod common;

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use bookrack_test_support::{ProcessEnv, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{
    Reader, Writer, await_channel, build_opts, connect, join_with_deadline, recv, send,
};

/// A dependency source location under a build-machine prefix.
const LOCATED: &str =
    "/disk/cache/cargo/registry/src/index.crates.io-0123456789abcdef/dep-1.0.0/src/train.rs:10:4";

/// The build-machine prefix [`LOCATED`] carries.
const PREFIX: &str = "/disk/cache/cargo/";

/// What [`LOCATED`] reads as once redacted.
const KEPT: &str = "dep-1.0.0/src/train.rs:10:4";

async fn call(
    reader: &mut Reader,
    w: &mut Writer,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    send(w, &req.to_string()).await?;
    recv(reader).await
}

/// Bring a daemon up with its queue worker, hand `body` the socket,
/// then shut it down.
async fn with_daemon<F, Fut>(body: F) -> Result<()>
where
    F: FnOnce(PathBuf) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    process_env(ProcessEnv::daemon());
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
        let result = body(sock.clone()).await;
        let (mut reader, mut w) = connect(&sock).await?;
        let _ = call(
            &mut reader,
            &mut w,
            u64::MAX,
            "daemon.shutdown",
            Value::Null,
        )
        .await?;
        result
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dependency_logging_a_location_reaches_logs_tail_and_the_log_event_redacted() -> Result<()>
{
    with_daemon(|sock| async move {
        let (mut sub_r, mut sub_w) = connect(&sock).await?;
        let resp = call(&mut sub_r, &mut sub_w, 1, "events.subscribe", Value::Null).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");

        tracing::warn!(
            target: "dep_test",
            "Could not infer vector index details for vector_idx: failed, {}: failed",
            LOCATED
        );

        let event = await_channel(&mut sub_r, "log", Duration::from_secs(10)).await?;
        let mut event = event;
        while event["params"]["value"]["target"] != "dep_test" {
            event = await_channel(&mut sub_r, "log", Duration::from_secs(10)).await?;
        }
        let message = event["params"]["value"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            message.contains(KEPT) && !message.contains(PREFIX),
            "the log event: {event}"
        );

        let (mut r, mut w) = connect(&sock).await?;
        let tail = call(&mut r, &mut w, 2, "logs.tail", json!({ "n": 500 })).await?;
        let logged = tail["result"]["events"]
            .as_array()
            .context("logs.tail returns events")?
            .iter()
            .find(|e| e["target"] == "dep_test")
            .cloned()
            .with_context(|| format!("the warning is in the ring: {tail}"))?;
        let message = logged["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(KEPT) && !message.contains(PREFIX),
            "logs.tail: {logged}"
        );
        Ok(())
    })
    .await
}

/// A queue document holding one failure recorded before redaction.
fn seeded_queue() -> Value {
    json!({
        "schema_version": bookrack_core::queue::QUEUE_SCHEMA_VERSION,
        "paused": false,
        "jobs": [{
            "id": "01900000-0000-7000-8000-000000000001",
            "library": "default",
            "path": "/srv/books/example.epub",
            "kind": "book",
            "priority": "normal",
            "force": false,
            "state": "failed",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": "2026-01-02T03:04:06Z",
            "finished_at": "2026-01-02T03:04:07Z",
            "error": format!("ingest: train failed, {LOCATED}: train failed"),
        }],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_recorded_before_redaction_reads_back_redacted_after_a_restart() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon());
    let queue_path = sandbox.daemon_state_dir().join("queue.json");
    std::fs::create_dir_all(sandbox.daemon_state_dir())?;
    std::fs::write(&queue_path, seeded_queue().to_string())?;

    with_daemon(move |sock| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let listed = call(&mut r, &mut w, 1, "queue.list", Value::Null).await?;
        let error = listed["result"]["jobs"][0]["error"]
            .as_str()
            .with_context(|| format!("the seeded failure is listed: {listed}"))?;
        assert_eq!(error, format!("ingest: train failed, {KEPT}: train failed"));

        let (mut sub_r, mut sub_w) = connect(&sock).await?;
        let resp = call(&mut sub_r, &mut sub_w, 2, "events.subscribe", Value::Null).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");
        let mut snapshot = String::new();
        for _ in 0..7 {
            snapshot.push_str(&recv(&mut sub_r).await?.to_string());
        }
        assert!(
            snapshot.contains(KEPT),
            "the snapshot lists the failure: {snapshot}"
        );
        assert!(!snapshot.contains(PREFIX), "{snapshot}");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let on_disk = std::fs::read_to_string(&queue_path)?;
            if !on_disk.contains(PREFIX) && on_disk.contains(KEPT) {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err(eyre!("the saved queue still carries the prefix: {on_disk}"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_book_quoting_a_dependency_location_is_returned_verbatim() -> Result<()> {
    with_daemon(|sock| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let sources = tempfile::tempdir()?;
        let book = sources.path().join("synthetic-manual.txt");
        let quoted = format!(
            "Chapter One\n\n\
             The synthetic manual reproduces a panic message in full, as an \
             example of what a build prints: thread main panicked at {LOCATED}. \
             The reader is asked to note the path and the line number.\n\n\
             The rest of the chapter describes a fictional ferry and its keeper, \
             who logs the crossings of each day in a fictional ledger.\n"
        );
        std::fs::write(&book, &quoted)?;

        let submitted = call(
            &mut r,
            &mut w,
            1,
            "ingest.submit",
            json!({ "paths": [book] }),
        )
        .await?;
        let job = submitted["result"]["job_ids"][0]
            .as_str()
            .with_context(|| format!("no job id: {submitted}"))?
            .to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(40);
        loop {
            let listed = call(&mut r, &mut w, 2, "queue.list", Value::Null).await?;
            let state = listed["result"]["jobs"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(&job)))
                .and_then(|row| row["state"].as_str())
                .unwrap_or("missing")
                .to_string();
            match state.as_str() {
                "done" => break,
                "pending" | "running" if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ => return Err(eyre!("the fixture ingest ended {state}: {listed}")),
            }
        }

        let found = call(
            &mut r,
            &mut w,
            3,
            "library.search",
            json!({ "query": "panic message path line number", "top_k": 50 }),
        )
        .await?;
        let hits = found["result"]
            .as_array()
            .with_context(|| format!("search returns hits: {found}"))?;
        assert!(
            hits.iter()
                .filter_map(|hit| hit["text"].as_str())
                .any(|text| text.contains(LOCATED)),
            "the quoted location comes back verbatim: {found}"
        );
        Ok(())
    })
    .await
}

/// A real LanceDB error raised through a public method: a search asking
/// for a zero refine factor over an indexed library. The envelope
/// keeps LanceDB's sentence and the crate-relative location — so a
/// location was there to redact — and drops the marker that only the
/// absolute prefix carries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lance_error_reaches_the_envelope_without_the_build_prefix() -> Result<()> {
    with_daemon(|sock| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let sources = tempfile::tempdir()?;
        let book = sources.path().join("synthetic-ferry.txt");
        std::fs::write(
            &book,
            "Chapter One\n\n\
             The synthetic ferry crossed the fictional river twice a day, and its \
             keeper logged every crossing in a fictional ledger.\n",
        )?;
        let submitted = call(
            &mut r,
            &mut w,
            1,
            "ingest.submit",
            json!({ "paths": [book] }),
        )
        .await?;
        let job = submitted["result"]["job_ids"][0]
            .as_str()
            .with_context(|| format!("no job id: {submitted}"))?
            .to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(40);
        loop {
            let listed = call(&mut r, &mut w, 2, "queue.list", Value::Null).await?;
            let state = listed["result"]["jobs"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(&job)))
                .and_then(|row| row["state"].as_str())
                .unwrap_or("missing")
                .to_string();
            match state.as_str() {
                "done" => break,
                "pending" | "running" if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ => return Err(eyre!("the fixture ingest ended {state}: {listed}")),
            }
        }

        let refused = call(
            &mut r,
            &mut w,
            3,
            "library.search",
            json!({ "query": "ferry", "refine_factor": 0 }),
        )
        .await?;
        let envelope = refused["error"].to_string();
        assert!(
            envelope.contains("Refine factor cannot be zero"),
            "LanceDB's own refusal: {refused}"
        );
        assert!(
            envelope.contains("/src/dataset/scanner.rs:"),
            "the crate-relative location stays: {refused}"
        );
        assert!(
            !envelope.contains("registry/src/"),
            "the build prefix is gone: {refused}"
        );
        Ok(())
    })
    .await
}
