// SPDX-License-Identifier: Apache-2.0

//! End-to-end check of the `worker.progress` contract: the queue
//! worker reports a pulled job as `processing`, tagged with the job's
//! pipeline `kind`, and names no pipeline stage it cannot observe. A
//! book job carried through the real ingest pipeline and a paper job
//! that fails on a missing source each publish exactly one
//! `processing` marker before their closing `queue.tick`; the job's
//! terminal state travels on that tick, not on `worker.progress`.
//!
//! The embedder probe and the embedding pass are both answered by the
//! loopback stub in `common`, so no Ollama daemon is required. The
//! paper job never reaches PDF parsing, so no PDFium library is
//! required either.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

/// A synthetic text long enough for the ingest pipeline to extract
/// prose, plan chunks, and embed them against the stub.
const BOOK_TEXT: &str = "\
Chapter One

The synthetic narrator opened the synthetic ledger and began to
count. Every entry in the ledger described a fictional object, and
every fictional object had a fictional weight. Counting the weights
took the narrator most of the morning.

By noon the ledger was balanced. The narrator celebrated with a
fictional meal and recorded the celebration as a footnote. Footnotes,
the narrator believed, were the highest form of bookkeeping.

Chapter Two

On the second day the ledger grew a second column. The new column
held colours instead of weights, and the narrator sorted them from
dull to bright. Sorting colours was slower than counting weights.

When the sorting ended the narrator closed the ledger, satisfied
that both columns agreed with each other in every fictional respect.
";

/// Read frames until the job's closing `queue.tick` arrives,
/// collecting every `worker.progress` payload published for it.
async fn collect_progress_until_done(
    reader: &mut Reader,
    job_id: &str,
    timeout: Duration,
) -> Result<(Vec<Value>, Value)> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut progress = Vec::new();
    loop {
        tokio::select! {
            _ = &mut deadline => {
                return Err(eyre!(
                    "timed out before the job's closing queue.tick; progress so far: {progress:?}"
                ));
            }
            frame = reader.next_line() => {
                let line = frame?.ok_or_else(|| eyre!("eof while collecting progress"))?;
                let v: Value = serde_json::from_str(&line)?;
                if v.get("method").is_none() {
                    continue;
                }
                let value = &v["params"]["value"];
                match v["params"]["channel"].as_str() {
                    Some("worker.progress") if value["job_id"].as_str() == Some(job_id) => {
                        progress.push(value.clone());
                    }
                    Some("queue.tick")
                        if value["last_finished"]["job_id"].as_str() == Some(job_id) =>
                    {
                        return Ok((progress, value["last_finished"].clone()));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Projects each `worker.progress` payload onto its `(stage, kind)`
/// pair, spelling an absent field as `<missing>` so a payload that
/// lacks one fails the comparison with the field named.
fn stage_kind_pairs(progress: &[Value]) -> Vec<(String, String)> {
    let field = |v: &Value, key: &str| v[key].as_str().unwrap_or("<missing>").to_string();
    progress
        .iter()
        .map(|v| (field(v, "stage"), field(v, "kind")))
        .collect()
}

/// Starts a daemon with a live queue worker, submits `path` through
/// `submit_method`, and returns every `worker.progress` payload for
/// the resulting job together with the job's closing `last_finished`.
async fn drive_one_job(submit_method: &'static str, path: PathBuf) -> Result<(Vec<Value>, Value)> {
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
    let (out_tx, out_rx) = tokio::sync::oneshot::channel();

    let driver = tokio::spawn(async move {
        // Observer first, so every event of the job below is captured.
        let (mut obs_reader, mut obs_w) = connect(&sock).await?;
        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#,
        )
        .await?;
        let resp = recv(&mut obs_reader).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");
        // Drain the snapshot bundle so only live broadcasts remain.
        for _ in 0..7 {
            let _ = recv(&mut obs_reader).await?;
        }

        let (mut wr_reader, mut wr_w) = connect(&sock).await?;
        let submit = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": submit_method,
            "params": {"paths": [path]},
        });
        send(&mut wr_w, &submit.to_string()).await?;
        let submit_resp = recv(&mut wr_reader).await?;
        let job_id = submit_resp["result"]["job_ids"][0]
            .as_str()
            .with_context(|| format!("missing job id: {submit_resp}"))?
            .to_string();

        let collected =
            collect_progress_until_done(&mut obs_reader, &job_id, Duration::from_secs(45)).await?;
        let _ = out_tx.send(collected);

        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut wr_reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;
    out_rx
        .await
        .map_err(|_| eyre!("driver finished without reporting the job's progress"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_book_job_reports_processing_and_no_stage_it_cannot_observe() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let book_dir = tempfile::tempdir()?;
    let book_path = book_dir.path().join("synthetic-ledger.txt");
    std::fs::write(&book_path, BOOK_TEXT)?;

    let (progress, last_finished) = drive_one_job("ingest.submit", book_path).await?;

    // The job must have succeeded, so the progress below covers a
    // full pipeline run rather than an early failure.
    assert_eq!(
        last_finished["state"].as_str(),
        Some("done"),
        "job did not finish clean: {last_finished}"
    );
    // One marker on pull, then the embedding pass's measured fraction;
    // nothing claims a stage the worker cannot see, and nothing repeats
    // the terminal state the closing tick already carries.
    let pairs = stage_kind_pairs(&progress);
    let (first, rest) = pairs
        .split_first()
        .with_context(|| format!("book job published no worker.progress: {progress:?}"))?;
    assert_eq!(
        first,
        &("processing".to_string(), "book".to_string()),
        "book job's first worker.progress is not the pull marker: {progress:?}"
    );
    assert!(
        !rest.is_empty(),
        "book job reported no embed progress: {progress:?}"
    );
    assert!(
        rest.iter()
            .all(|pair| pair == &("embed".to_string(), "book".to_string())),
        "book job reported something other than embed after the pull marker: {progress:?}"
    );
    let fractions: Vec<f64> = progress[1..]
        .iter()
        .map(|v| v["stage_progress"].as_f64().unwrap_or(f64::NAN))
        .collect();
    assert!(
        fractions.iter().all(|f| (0.0..=1.0).contains(f)),
        "embed progress outside 0.0..=1.0 or missing: {fractions:?}"
    );
    assert_eq!(
        fractions.last().copied(),
        Some(1.0),
        "the last batch was not reported: {fractions:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_job_reports_processing_under_its_own_kind() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let paper_dir = tempfile::tempdir()?;
    // The marker is published when the worker pulls the job, before
    // the pipeline opens the source, so a missing file still emits it
    // and the job then fails on its own.
    let paper_path = paper_dir.path().join("absent-paper.pdf");

    let (progress, last_finished) = drive_one_job("glean.submit", paper_path).await?;

    assert_eq!(
        last_finished["kind"].as_str(),
        Some("paper"),
        "closing tick is not for a paper job: {last_finished}"
    );
    assert_eq!(
        last_finished["state"].as_str(),
        Some("failed"),
        "a paper job on a missing source must fail: {last_finished}"
    );
    // A paper job is reported under its own kind and never with a
    // book-pipeline stage name.
    assert_eq!(
        stage_kind_pairs(&progress),
        vec![("processing".to_string(), "paper".to_string())],
        "paper job's worker.progress drifted from the documented contract: {progress:?}"
    );
    Ok(())
}
