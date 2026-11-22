// SPDX-License-Identifier: Apache-2.0

//! End-to-end check of the queue worker's `library.changed`: every job
//! the worker settles announces the job's library once, ahead of the
//! `queue.tick` that closes the job, whatever the job's outcome, and
//! without pausing MCP. A job cancelled while it is still pending never
//! reaches the worker and announces nothing.
//!
//! The embedder probe and the embedding pass are both answered by the
//! loopback stub the daemon spec installs, so no Ollama daemon is
//! required.

#![cfg(unix)]

use std::time::Duration;

use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

/// A synthetic book long enough for the ingest pipeline to extract
/// prose, plan chunks, and embed them against the stub.
const BOOK_TEXT: &str = "\
Chapter One

The synthetic archivist opened the synthetic drawer and began to
label. Every folder in the drawer held a fictional letter, and every
fictional letter carried a fictional date. Labelling the dates took
the archivist most of the afternoon.

Chapter Two

On the second day the drawer grew a second row. The new row held
maps instead of letters, and the archivist sorted them from north to
south. Sorting maps was slower than labelling dates.
";

/// A synthetic paper whose abstract is one chunk.
const PAPER_TEXT: &str = "\
A Synthetic Paper

Abstract. This synthetic paper describes a deliberately fictional study
of how a queue worker announces the library a settled job wrote, in
enough words for the abstract pick to accept the paragraph.

The first section restates the fictional problem in plain terms.
";

/// What the observer saw for one job, from its submit to its closing
/// `queue.tick`.
struct JobWindow {
    /// The `library` of every `library.changed` in the window.
    announced: Vec<String>,
    /// The `paused` of every `mcp.availability` in the window.
    availability: Vec<bool>,
    /// The closing tick's `last_finished`.
    last_finished: Value,
}

/// Send one request on the writer connection and return its reply.
async fn call(
    r: &mut Reader,
    w: &mut Writer,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    send(w, &request.to_string()).await?;
    let reply = recv(r).await?;
    if reply.get("error").is_some() {
        return Err(eyre!("{method} failed: {reply}"));
    }
    Ok(reply["result"].clone())
}

/// Read the observer's frames until the `queue.tick` closing `job_id`,
/// keeping the `library.changed` and `mcp.availability` payloads seen
/// on the way.
async fn watch_job(obs: &mut Reader, job_id: &str) -> Result<JobWindow> {
    let deadline = tokio::time::sleep(Duration::from_secs(45));
    tokio::pin!(deadline);
    let mut announced = Vec::new();
    let mut availability = Vec::new();
    loop {
        tokio::select! {
            _ = &mut deadline => {
                return Err(eyre!(
                    "timed out before job {job_id} closed; announced so far: {announced:?}"
                ));
            }
            frame = obs.next_line() => {
                let line = frame?.ok_or_else(|| eyre!("eof while watching job {job_id}"))?;
                let v: Value = serde_json::from_str(&line)?;
                let value = &v["params"]["value"];
                match v["params"]["channel"].as_str() {
                    Some("library.changed") => announced.push(
                        value["library"].as_str().unwrap_or("<missing>").to_string(),
                    ),
                    Some("mcp.availability") => {
                        availability.push(value["paused"].as_bool().unwrap_or(true));
                    }
                    Some("queue.tick")
                        if value["last_finished"]["job_id"].as_str() == Some(job_id) =>
                    {
                        return Ok(JobWindow {
                            announced,
                            availability,
                            last_finished: value["last_finished"].clone(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The first job id a submit reply names.
fn job_id(result: &Value) -> Result<String> {
    result["job_ids"][0]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("no job id in the submit reply: {result}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_settled_job_announces_its_library_and_a_pending_cancel_does_not() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let sources = tempfile::tempdir()?;
    let book = sources.path().join("synthetic-archive.txt");
    std::fs::write(&book, BOOK_TEXT)?;
    let cancelled_book = sources.path().join("synthetic-cancelled.txt");
    std::fs::write(&cancelled_book, BOOK_TEXT.replace("archivist", "clerk"))?;
    let paper = sources.path().join("synthetic-paper.txt");
    std::fs::write(&paper, PAPER_TEXT)?;
    let absent_paper = sources.path().join("absent-paper.pdf");

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
        let (mut obs, mut obs_w) = connect(&sock).await?;
        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#,
        )
        .await?;
        let resp = recv(&mut obs).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");
        // Drain the snapshot bundle so only live broadcasts remain.
        for _ in 0..7 {
            let _ = recv(&mut obs).await?;
        }

        let (mut r, mut w) = connect(&sock).await?;
        let libraries = call(&mut r, &mut w, 2, "library.list", Value::Null).await?;
        let library = libraries[0]["name"]
            .as_str()
            .with_context(|| format!("no library served: {libraries}"))?
            .to_string();

        // A job cancelled while pending: the worker never pulls it.
        call(&mut r, &mut w, 3, "queue.pause", Value::Null).await?;
        let submitted = call(
            &mut r,
            &mut w,
            4,
            "ingest.submit",
            json!({ "paths": [cancelled_book] }),
        )
        .await?;
        let cancelled = job_id(&submitted)?;
        call(
            &mut r,
            &mut w,
            5,
            "ingest.cancel",
            json!({ "job_id": cancelled }),
        )
        .await?;
        call(&mut r, &mut w, 6, "queue.resume", Value::Null).await?;

        // Four jobs the worker settles, one outcome each.
        let legs: [(&str, Value, &str); 4] = [
            ("ingest.submit", json!({ "paths": [book] }), "done"),
            (
                "ingest.submit",
                json!({ "paths": [book] }),
                "skipped_duplicate",
            ),
            ("glean.submit", json!({ "paths": [paper] }), "done"),
            ("glean.submit", json!({ "paths": [absent_paper] }), "failed"),
        ];
        for (n, (method, params, state)) in legs.into_iter().enumerate() {
            let submitted = call(&mut r, &mut w, 10 + n as u64, method, params).await?;
            let id = job_id(&submitted)?;
            let window = watch_job(&mut obs, &id).await?;
            assert_eq!(
                window.last_finished["state"].as_str(),
                Some(state),
                "leg {n} ({method}) closed in another state: {}",
                window.last_finished
            );
            // The first leg's window also spans the pending cancel and
            // the resume, so a cancelled job that announced would show
            // here as a second entry.
            assert_eq!(
                window.announced,
                [library.as_str()],
                "leg {n} ({method}, {state}) must announce its library once, \
                 before its closing tick"
            );
            assert!(
                !window.availability.contains(&true),
                "a queue job must not pause MCP: leg {n} saw {:?}",
                window.availability
            );
        }

        call(&mut r, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
