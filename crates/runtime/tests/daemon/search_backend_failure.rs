// SPDX-License-Identifier: Apache-2.0

//! A search tells the caller what went wrong with its own embed round
//! trip, and with its query, through the code the failure deserves.
//!
//! The query a search embeds goes through the search layer, so an
//! embed failure reaches the control socket three wrappers deep. An
//! Ollama that answered "overloaded" is a backend the same call may
//! reach later; one that does not hold the model is the operator's
//! configuration, with `ollama pull` as the step. An empty query is the
//! caller's input whatever state the library is in: a side with no
//! vector store, and an intake no catalog registers, refuse it the
//! same way as a side that could have searched.
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

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

const INVALID_PARAMS: i64 = -32602;
const BACKEND_UNAVAILABLE: i64 = -32017;

/// An id no intake in this fixture carries.
const PHANTOM: i64 = 999_999;

fn write_source(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("synthetic-almanac.txt");
    std::fs::write(
        &path,
        "Chapter One\n\n\
         The synthetic almanac listed the tides of a fictional harbour. \
         Every tide had a fictional height, and every height was written \
         beside the hour the fictional keeper had measured it.\n\n\
         By evening the almanac was complete. The keeper closed it and \
         recorded the closing as a footnote.\n",
    )?;
    Ok(path)
}

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

/// Submit one book and wait for its job to be `done`.
async fn fill_one(reader: &mut Reader, w: &mut Writer, path: &Path) -> Result<()> {
    let resp = call(reader, w, 1, "ingest.submit", json!({ "paths": [path] })).await?;
    let job = resp["result"]["job_ids"][0]
        .as_str()
        .with_context(|| format!("no job id: {resp}"))?
        .to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        let listed = call(reader, w, 2, "queue.list", Value::Null).await?;
        let state = listed["result"]["jobs"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(&job)))
            .and_then(|row| row["state"].as_str())
            .unwrap_or("missing")
            .to_string();
        match state.as_str() {
            "done" => return Ok(()),
            "failed" | "cancelled" | "missing" => {
                return Err(eyre!("the fixture job did not finish clean: {listed}"));
            }
            _ if std::time::Instant::now() > deadline => {
                return Err(eyre!("the fixture job is still {state}"));
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// The searches that embed `query` against the book the fixture
/// ingested: the book side, both sides at once, and the one book.
fn book_searches(query: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("library.search", json!({ "query": query, "kind": "book" })),
        ("library.search", json!({ "query": query, "kind": "all" })),
        (
            "library.search_in_book",
            json!({ "query": query, "intake_id": 1 }),
        ),
    ]
}

/// Run every search in `searches` with the stub switched to `failure`,
/// and switch it back before any reply is judged, so a failed
/// assertion cannot leave the stub failing.
async fn replies_under(
    reader: &mut Reader,
    w: &mut Writer,
    failure: EmbedFailure,
    searches: Vec<(&'static str, Value)>,
) -> Vec<(String, Result<Value>)> {
    EmbedStub::shared().set_failure(failure);
    let mut replies = Vec::new();
    for (id, (method, params)) in (10u64..).zip(searches) {
        let label = format!("{method} {params}");
        replies.push((label, call(reader, w, id, method, params).await));
    }
    EmbedStub::shared().set_failure(EmbedFailure::None);
    replies
}

/// The `error` of a reply that must have failed with `code`.
fn error_with(label: &str, reply: Result<Value>, code: i64) -> Result<Value> {
    let reply = reply?;
    let error = &reply["error"];
    if error.is_null() {
        return Err(eyre!("{label} was expected to fail: {reply}"));
    }
    if error["code"].as_i64() != Some(code) {
        return Err(eyre!("{label}: expected code {code}, got {error}"));
    }
    Ok(error.clone())
}

/// The summary a failure carries must be the leaf's, not a wrapper's.
fn assert_no_wrapper_name(label: &str, error: &Value) {
    let message = error["message"].as_str().unwrap_or_default();
    for wrapper in ["search error", "embed error", "query error"] {
        assert!(
            !message.contains(wrapper),
            "{label}: a wrapper's name is not a failure: {message}"
        );
    }
}

async fn a_search_reports_its_backend_and_its_query_with_their_own_codes(
    sock: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, &write_source(sources.path())?).await?;

    // A healthy backend answers every one of the searches below, so a
    // failure further down is the switch's doing.
    for (label, reply) in
        replies_under(&mut r, &mut w, EmbedFailure::None, book_searches("tides")).await
    {
        let reply = reply?;
        let hits = reply["result"]
            .as_array()
            .with_context(|| format!("{label}: no result: {reply}"))?;
        assert!(
            !hits.is_empty(),
            "{label}: the fixture book is found: {reply}"
        );
    }

    let overloaded = replies_under(
        &mut r,
        &mut w,
        EmbedFailure::Overloaded,
        book_searches("tides"),
    )
    .await;
    for (label, reply) in overloaded {
        let error = error_with(&label, reply, BACKEND_UNAVAILABLE)?;
        assert_eq!(error["data"]["retryable"], json!(true), "{label}: {error}");
        assert!(
            error["data"]["hint"]
                .as_str()
                .is_some_and(|h| !h.is_empty()),
            "{label}: the leaf's hint is lost: {error}"
        );
        assert_no_wrapper_name(&label, &error);
    }

    let unpulled = replies_under(
        &mut r,
        &mut w,
        EmbedFailure::ModelNotFound,
        book_searches("tides"),
    )
    .await;
    for (label, reply) in unpulled {
        let error = error_with(&label, reply, INVALID_PARAMS)?;
        let hint = error["data"]["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("ollama pull"), "{label}: {error}");
        assert_eq!(error["data"]["retryable"], json!(false), "{label}: {error}");
        assert_no_wrapper_name(&label, &error);
    }

    // The paper side holds no vector store, and the phantom ids name no
    // intake: an empty query is refused there exactly as on the book
    // the fixture ingested.
    let mut empty = book_searches("   ");
    empty.extend([
        ("library.search", json!({ "query": "   ", "kind": "paper" })),
        (
            "library.search_in_book",
            json!({ "query": "", "intake_id": PHANTOM }),
        ),
        (
            "library.search_in_paper",
            json!({ "query": "", "intake_id": PHANTOM }),
        ),
    ]);
    for (label, reply) in replies_under(&mut r, &mut w, EmbedFailure::None, empty).await {
        let error = error_with(&label, reply, INVALID_PARAMS)?;
        assert_eq!(error["message"], json!("query text is empty"), "{label}");
        assert!(
            error["data"]["hint"]
                .as_str()
                .is_some_and(|h| !h.is_empty()),
            "{label}: an empty query has a next step: {error}"
        );
        assert_eq!(error["data"]["retryable"], json!(false), "{label}: {error}");
    }
    Ok(())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_search_reports_its_embed_backend_and_its_query_with_their_own_codes() -> Result<()> {
    with_daemon(a_search_reports_its_backend_and_its_query_with_their_own_codes).await
}
