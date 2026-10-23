// SPDX-License-Identifier: Apache-2.0

//! A queue job that fails on a typed error records the control plane's
//! diagnostic beside its flattened text.
//!
//! The failure is the embed backend's: the stub answers 404 for the
//! model, so the ingest fails on `EmbedError::ModelNotFound`, the one
//! error whose hint names `ollama pull`. The row `queue.list` returns
//! carries `problem`; the `queue.tick` that closed the job carries the
//! same object; a retry clears it, and the rerun that succeeds leaves it
//! clear.
//!
//! The daemon reads its embedder URL from the process environment, so
//! the switch this file flips is the shared stub's and it relies on the
//! per-test process `cargo nextest` runs it in.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_test_support::{EmbedFailure, EmbedStub, ProcessEnv, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{
    Reader, Writer, await_channel, build_opts, connect, join_with_deadline, recv, send,
};

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

/// Submit one book and return its job id.
async fn submit_one(reader: &mut Reader, w: &mut Writer, path: &Path) -> Result<String> {
    let resp = call(reader, w, 1, "ingest.submit", json!({ "paths": [path] })).await?;
    Ok(resp["result"]["job_ids"][0]
        .as_str()
        .with_context(|| format!("no job id: {resp}"))?
        .to_string())
}

/// The listed row for `id`.
async fn row_of(reader: &mut Reader, w: &mut Writer, id: &str) -> Result<Value> {
    let listed = call(reader, w, 2, "queue.list", Value::Null).await?;
    listed["result"]["jobs"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(id)))
        .cloned()
        .with_context(|| format!("job {id} is not listed: {listed}"))
}

/// Poll `queue.list` until the job `id` leaves `pending` / `running`,
/// and return its row.
async fn wait_for(reader: &mut Reader, w: &mut Writer, id: &str) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let row = row_of(reader, w, id).await?;
        match row["state"].as_str() {
            Some("pending" | "running") if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Some("pending" | "running") => return Err(eyre!("the job is still open: {row}")),
            _ => return Ok(row),
        }
    }
}

/// The next `queue.tick` whose `last_finished` names `id`.
async fn tick_closing(reader: &mut Reader, id: &str) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| eyre!("no queue.tick closed job {id}"))?;
        let frame = await_channel(reader, "queue.tick", remaining).await?;
        if frame["params"]["value"]["last_finished"]["job_id"].as_str() == Some(id) {
            return Ok(frame["params"]["value"]["last_finished"].clone());
        }
    }
}

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

/// Every string value in `value`, at any depth.
fn strings_of(value: &Value) -> Vec<&str> {
    match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(items) => items.iter().flat_map(strings_of).collect(),
        Value::Object(fields) => fields.values().flat_map(strings_of).collect(),
        _ => Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_job_lists_its_problem_and_the_tick_carries_the_same() -> Result<()> {
    with_daemon(|sock| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let (mut sub_r, mut sub_w) = connect(&sock).await?;
        let resp = call(&mut sub_r, &mut sub_w, 9, "events.subscribe", Value::Null).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");

        let sources = tempfile::tempdir()?;
        let book = sources.path().join("synthetic-almanac.txt");
        std::fs::write(
            &book,
            "Chapter One\n\n\
             The synthetic almanac listed every fictional tide of the year, and \
             the keeper of the almanac checked each one against a fictional moon.\n",
        )?;

        EmbedStub::shared().set_failure(EmbedFailure::ModelNotFound);
        let id = submit_one(&mut r, &mut w, &book).await?;
        let row = wait_for(&mut r, &mut w, &id).await?;
        assert_eq!(row["state"], "failed", "{row}");
        let problem = &row["problem"];
        assert!(
            problem.is_object(),
            "a failure the control plane classifies records its diagnostic: {row}"
        );
        let hint = problem["hint"].as_str().unwrap_or_default();
        assert!(
            hint.contains("ollama pull"),
            "the hint is the embed error's own next step: {problem}"
        );
        assert_eq!(problem["retryable"], Value::Bool(false), "{problem}");
        let summary = problem["summary"].as_str().unwrap_or_default();
        assert!(!summary.is_empty(), "{problem}");
        assert_ne!(Some(summary), row["error"].as_str(), "{row}");
        for text in strings_of(problem) {
            assert!(
                !text.contains("registry/src/"),
                "no build-machine location in the diagnostic: {problem}"
            );
        }

        let closing = tick_closing(&mut sub_r, &id).await?;
        assert_eq!(
            closing["problem"], *problem,
            "the tick that closed the job carries the listed diagnostic: {closing}"
        );

        EmbedStub::shared().set_failure(EmbedFailure::None);
        let paused = call(&mut r, &mut w, 3, "queue.pause", Value::Null).await?;
        assert_eq!(paused["result"]["paused"], Value::Bool(true), "{paused}");
        let retried = call(&mut r, &mut w, 4, "queue.retry", json!({ "job_id": id })).await?;
        assert_eq!(retried["result"]["retried"], json!([id]), "{retried}");
        let pending = row_of(&mut r, &mut w, &id).await?;
        assert_eq!(pending["state"], "pending", "{pending}");
        assert_eq!(
            pending["problem"],
            Value::Null,
            "a retry clears the diagnostic: {pending}"
        );
        assert_eq!(pending["error"], Value::Null, "{pending}");

        let resumed = call(&mut r, &mut w, 5, "queue.resume", Value::Null).await?;
        assert_eq!(resumed["result"]["paused"], Value::Bool(false), "{resumed}");
        let rerun = wait_for(&mut r, &mut w, &id).await?;
        assert!(
            matches!(rerun["state"].as_str(), Some("done" | "skipped_duplicate")),
            "the rerun against a healthy backend succeeds: {rerun}"
        );
        assert_eq!(rerun["problem"], Value::Null, "{rerun}");
        assert_eq!(rerun["error"], Value::Null, "{rerun}");
        Ok(())
    })
    .await
}
