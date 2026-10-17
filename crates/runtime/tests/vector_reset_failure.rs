// SPDX-License-Identifier: Apache-2.0

//! A vector reset that fails tells the caller whether resuming is the
//! next step.
//!
//! A failure inside the reset pass leaves finished items at `Embedded`
//! and the rest at `Extracted`, so the error's hint gains the resumed
//! rerun after whatever step the cause itself calls for. A failure
//! while the reset is being set up changed nothing — a resume there
//! would skip a reset that never ran — so its hint is left alone and
//! no run is registered.
//!
//! Lives in its own binary because `EmbedStub::set_failure` is
//! process-global.

#![cfg(unix)]

mod common;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::Catalog;
use bookrack_config::{LibraryKind, ManifestIdentitySeed, set_manifest_index_profile};
use bookrack_test_support::{EmbedFailure, EmbedStub, ProcessEnv, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

/// One pipeline's reset surface.
#[derive(Clone, Copy)]
struct Side {
    submit: &'static str,
    reset: &'static str,
    reembed: &'static str,
    catalog: &'static str,
    /// The `pipeline_runs.command` a reset registers under.
    reset_run: &'static str,
}

const BOOKS: Side = Side {
    submit: "ingest.submit",
    reset: "vectors.reset",
    reembed: "vectors.reembed",
    catalog: "catalog.db",
    reset_run: "reset",
};

const PAPERS: Side = Side {
    submit: "glean.submit",
    reset: "papers.vectors_reset",
    reembed: "papers.vectors_reembed",
    catalog: "papers_catalog.db",
    reset_run: "papers_reset",
};

fn write_source(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("synthetic-ledger.txt");
    std::fs::write(
        &path,
        "Chapter One\n\n\
         The synthetic narrator opened the synthetic ledger and began to \
         count. Every entry in the ledger described a fictional object, and \
         every fictional object had a fictional weight. Counting the weights \
         took the narrator most of the morning.\n\n\
         By noon the ledger was balanced. The narrator celebrated with a \
         fictional meal and recorded the celebration as a footnote.\n",
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

/// The `error` of a call that must fail.
async fn error_of(
    reader: &mut Reader,
    w: &mut Writer,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let resp = call(reader, w, id, method, params).await?;
    if resp["error"].is_null() {
        return Err(eyre!("{method} was expected to fail: {resp}"));
    }
    Ok(resp["error"].clone())
}

/// Submit one source through `side` and wait for its job to be `done`.
async fn fill_one(reader: &mut Reader, w: &mut Writer, side: Side, path: &Path) -> Result<()> {
    let resp = call(reader, w, 1, side.submit, json!({ "paths": [path] })).await?;
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

fn reset_runs(root: &Path, side: Side) -> Result<usize> {
    Ok(Catalog::open(&root.join(side.catalog))?
        .list_pipeline_runs(None, None)?
        .into_iter()
        .filter(|run| run.command == side.reset_run)
        .count())
}

async fn with_daemon<F, Fut>(body: F) -> Result<()>
where
    F: FnOnce(PathBuf, PathBuf) -> Fut + Send + 'static,
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
    let root = data_root.path().to_path_buf();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let result = body(sock.clone(), root).await;
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

/// The embedder goes down partway through the pass. The envelope is
/// the one the same backend failure takes on a re-embed, a write with
/// no reset behind it — same code, summary, evidence, and retry
/// judgement — and its hint keeps that failure's own step first and
/// adds the resumed rerun after it.
async fn a_reset_interrupted_mid_pass_adds_the_resume_step(
    side: Side,
    sock: PathBuf,
    _root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, side, &write_source(sources.path())?).await?;

    // The dimension probe is answered from a process-wide cache once
    // the daemon has made it, so the comparison write has to embed
    // chunk text for real: the re-embed execute leg does.
    let plan = call(&mut r, &mut w, 3, side.reembed, json!({ "dry_run": true })).await?;
    let plan_id = plan["result"]["plan_id"]
        .as_str()
        .with_context(|| format!("no plan id: {plan}"))?
        .to_string();
    EmbedStub::set_failure(EmbedFailure::Overloaded);
    let plain = error_of(
        &mut r,
        &mut w,
        4,
        side.reembed,
        json!({ "yes": true, "plan_id": plan_id }),
    )
    .await?;
    let reset = error_of(&mut r, &mut w, 5, side.reset, json!({ "yes": true })).await?;
    EmbedStub::set_failure(EmbedFailure::None);

    assert_eq!(reset["code"], plain["code"], "{reset} vs {plain}");
    assert_eq!(reset["message"], plain["message"], "{reset} vs {plain}");
    assert_eq!(reset["data"]["detail"], plain["data"]["detail"]);
    assert_eq!(reset["data"]["retryable"], plain["data"]["retryable"]);
    let own_step = plain["data"]["hint"]
        .as_str()
        .with_context(|| format!("the backend failure writes its own hint: {plain}"))?;
    let hint = reset["data"]["hint"]
        .as_str()
        .with_context(|| format!("no hint: {reset}"))?;
    assert!(
        hint.starts_with(own_step) && hint.len() > own_step.len(),
        "{}: the failure's own step comes first, the resume after it: {hint:?}",
        side.reset
    );
    assert!(hint.contains("--resume"), "{}: {hint:?}", side.reset);
    Ok(())
}

/// The reset cannot resolve its embedding configuration, so nothing
/// was changed: no resume is offered and no run is registered.
async fn a_reset_that_never_started_offers_no_resume(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    set_manifest_index_profile(
        &root,
        Some("no-such-profile"),
        ManifestIdentitySeed {
            name: "default",
            kind: LibraryKind::Test,
            description: None,
        },
    )?;
    let (mut r, mut w) = connect(&sock).await?;
    let err = error_of(&mut r, &mut w, 1, side.reset, json!({ "yes": true })).await?;
    assert_eq!(
        err["code"].as_i64(),
        Some(-32602),
        "{}: an undefined profile is caller input: {err}",
        side.reset
    );
    let hint = err["data"]["hint"].as_str().unwrap_or_default();
    assert!(
        !hint.contains("--resume"),
        "{}: nothing ran, so there is nothing to resume: {err}",
        side.reset
    );
    assert_eq!(reset_runs(&root, side)?, 0, "{}", side.reset);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_interrupted_mid_pass_adds_the_resume_step() -> Result<()> {
    with_daemon(|sock, root| a_reset_interrupted_mid_pass_adds_the_resume_step(BOOKS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_interrupted_mid_pass_adds_the_resume_step() -> Result<()> {
    with_daemon(|sock, root| a_reset_interrupted_mid_pass_adds_the_resume_step(PAPERS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_that_never_started_offers_no_resume() -> Result<()> {
    with_daemon(|sock, root| a_reset_that_never_started_offers_no_resume(BOOKS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_that_never_started_offers_no_resume() -> Result<()> {
    with_daemon(|sock, root| a_reset_that_never_started_offers_no_resume(PAPERS, sock, root)).await
}
