// SPDX-License-Identifier: Apache-2.0

//! `ingest.submit` keeps `hold_for_metadata` only as a retired
//! parameter: `true` is refused as caller input in either submission
//! form, before a job is queued, and `false` — the value every earlier
//! client sends on a plain ingest — is accepted and ignored.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use bookrack_test_support::{ProcessEnv, process_env};
use eyre::Result;
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

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

fn write_source(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("retired-hold-sample.txt");
    std::fs::write(
        &path,
        "The first paragraph of a short synthetic document about lighthouses.\n\
         A second paragraph carrying more prose to chunk and embed.\n",
    )?;
    Ok(path)
}

/// Bring a daemon up with its queue paused, so a submission stays
/// listed as it was queued, hand `body` the socket, then shut it down.
async fn with_paused_daemon<F, Fut>(body: F) -> Result<()>
where
    F: FnOnce(PathBuf) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
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
        let (mut reader, mut w) = connect(&sock).await?;
        let paused = call(&mut reader, &mut w, 1, "queue.pause", Value::Null).await?;
        assert_eq!(paused["result"]["paused"], true, "{paused}");
        let result = body(sock.clone()).await;
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

/// The number of jobs the queue holds.
async fn queued(reader: &mut Reader, w: &mut Writer) -> Result<usize> {
    let listed = call(reader, w, 90, "queue.list", Value::Null).await?;
    Ok(listed["result"]["jobs"]
        .as_array()
        .map(Vec::len)
        .unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_submission_asking_for_the_retired_hold_is_refused() -> Result<()> {
    with_paused_daemon(|sock| async move {
        let sources = tempfile::tempdir()?;
        let path = write_source(sources.path())?;
        let (mut r, mut w) = connect(&sock).await?;
        let refused = call(
            &mut r,
            &mut w,
            2,
            "ingest.submit",
            json!({ "paths": [path], "hold_for_metadata": true }),
        )
        .await?;
        assert_eq!(refused["error"]["code"], -32602, "{refused}");
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("`hold_for_metadata` is no longer accepted"),
            "{refused}"
        );
        assert!(
            refused["error"]["data"]["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("bookrack dryrun")),
            "the refusal names a next step: {refused}"
        );
        assert_eq!(queued(&mut r, &mut w).await?, 0, "the refusal queued a job");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_submission_sending_the_retired_hold_as_false_is_accepted() -> Result<()> {
    with_paused_daemon(|sock| async move {
        let sources = tempfile::tempdir()?;
        let path = write_source(sources.path())?;
        let (mut r, mut w) = connect(&sock).await?;
        let accepted = call(
            &mut r,
            &mut w,
            2,
            "ingest.submit",
            json!({ "paths": [path], "hold_for_metadata": false }),
        )
        .await?;
        assert!(accepted["error"].is_null(), "{accepted}");
        assert_eq!(
            accepted["result"]["job_ids"].as_array().map(Vec::len),
            Some(1),
            "{accepted}"
        );
        assert_eq!(queued(&mut r, &mut w).await?, 1);
        Ok(())
    })
    .await
}
