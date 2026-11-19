// SPDX-License-Identifier: Apache-2.0

//! End-to-end check of the `worker.progress` contract: the queue
//! worker reports a pulled job as `processing`, tagged with the job's
//! pipeline `kind`, and names no pipeline stage it cannot observe. A
//! book job carried through the real ingest pipeline, a rule job
//! carried through the same book pipeline, and a paper job with an
//! abstract to embed each publish one `processing` marker and then the
//! embedding pass's fraction. A paper job that fails on a missing
//! source, and one whose paper has no abstract, publish the marker
//! alone. The job's terminal state travels on its closing `queue.tick`,
//! not on `worker.progress`.
//!
//! The embedder probe and the embedding pass are both answered by the
//! loopback stub in `common`, so no Ollama daemon is required. One case
//! parses a PDF, and is skipped where PDFium is not available.

#![cfg(unix)]

use std::time::Duration;

use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{EmbedFailure, EmbedStub, ProcessEnv, process_env};

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

/// Starts a daemon with a live queue worker, submits one job through
/// `submit_method` with `params`, and returns every `worker.progress`
/// payload for the resulting job together with the job's closing
/// `last_finished`. The submit reply is read as `job_ids[0]` or, for a
/// single-job method, `job_id`.
async fn drive_one_job(submit_method: &'static str, params: Value) -> Result<(Vec<Value>, Value)> {
    let mut outcomes = drive_jobs(submit_method, vec![params], || {}).await?;
    outcomes.pop().context("one job was driven")
}

/// [`drive_one_job`] for several jobs on one daemon and one data root,
/// each submitted once the one before it has closed. `before_last`
/// runs after every job but the last has closed, ahead of the last
/// submit. Returns each job's progress and closing `last_finished`, in
/// submit order.
async fn drive_jobs(
    submit_method: &'static str,
    jobs: Vec<Value>,
    before_last: fn(),
) -> Result<Vec<(Vec<Value>, Value)>> {
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
        let last = jobs.len().saturating_sub(1);
        let mut collected = Vec::new();
        for (index, params) in jobs.into_iter().enumerate() {
            if index == last && index > 0 {
                before_last();
            }
            let submit = json!({
                "jsonrpc": "2.0",
                "id": 2 + index,
                "method": submit_method,
                "params": params,
            });
            send(&mut wr_w, &submit.to_string()).await?;
            let submit_resp = recv(&mut wr_reader).await?;
            let result = &submit_resp["result"];
            let job_id = result["job_ids"][0]
                .as_str()
                .or_else(|| result["job_id"].as_str())
                .with_context(|| format!("missing job id: {submit_resp}"))?
                .to_string();
            collected.push(
                collect_progress_until_done(&mut obs_reader, &job_id, Duration::from_secs(45))
                    .await?,
            );
        }
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

/// Submits `path` through `submit_method`, then again with `force`
/// while the embed backend answers every request with an error, and
/// returns the forced job's progress and closing `last_finished`.
async fn drive_a_forced_resubmit_with_the_backend_failing(
    submit_method: &'static str,
    path: &std::path::Path,
) -> Result<(Vec<Value>, Value)> {
    let outcomes = drive_jobs(
        submit_method,
        vec![
            json!({"paths": [path]}),
            json!({"paths": [path], "force": true}),
        ],
        || EmbedStub::shared().set_failure(EmbedFailure::ModelNotFound),
    )
    .await;
    EmbedStub::shared().set_failure(EmbedFailure::None);
    outcomes?.pop().context("two jobs were driven")
}

/// A forced resubmit of an unchanged book finds every chunk's vector
/// stored: the job needs no embed backend, and still reports the
/// embedding pass as complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_resubmit_of_an_unchanged_book_completes_without_the_backend() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let book_dir = tempfile::tempdir()?;
    let book_path = book_dir.path().join("synthetic-ledger.txt");
    std::fs::write(&book_path, BOOK_TEXT)?;

    let (progress, last_finished) =
        drive_a_forced_resubmit_with_the_backend_failing("ingest.submit", &book_path).await?;
    assert_embed_progress("forced book", "book", &progress, &last_finished)
}

/// The same for a paper.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_resubmit_of_an_unchanged_paper_completes_without_the_backend() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let paper_dir = tempfile::tempdir()?;
    let paper_path = paper_dir.path().join("synthetic-paper.txt");
    std::fs::write(&paper_path, PAPER_TEXT)?;

    let (progress, last_finished) =
        drive_a_forced_resubmit_with_the_backend_failing("glean.submit", &paper_path).await?;
    assert_embed_progress("forced paper", "paper", &progress, &last_finished)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_book_job_reports_processing_and_no_stage_it_cannot_observe() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let book_dir = tempfile::tempdir()?;
    let book_path = book_dir.path().join("synthetic-ledger.txt");
    std::fs::write(&book_path, BOOK_TEXT)?;

    let (progress, last_finished) =
        drive_one_job("ingest.submit", json!({"paths": [book_path]})).await?;
    assert_embed_progress("book", "book", &progress, &last_finished)
}

/// A synthetic paper whose abstract is one chunk.
const PAPER_TEXT: &str = "\
A Synthetic Paper

Abstract. This synthetic paper describes a deliberately fictional study
of how a queue worker reports the embedding of one abstract, in enough
words for the abstract pick to accept the paragraph.

The first section restates the fictional problem in plain terms.
";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_job_reports_embed_progress_under_its_own_kind() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let paper_dir = tempfile::tempdir()?;
    let paper_path = paper_dir.path().join("synthetic-paper.txt");
    std::fs::write(&paper_path, PAPER_TEXT)?;

    let (progress, last_finished) =
        drive_one_job("glean.submit", json!({"paths": [paper_path]})).await?;
    assert_embed_progress("paper", "paper", &progress, &last_finished)
}

/// Asserts the progress a job that embeds chunks publishes: one
/// `processing` marker on pull, then the embedding pass's measured
/// fraction, both under `kind`.
fn assert_embed_progress(
    what: &str,
    kind: &str,
    progress: &[Value],
    last_finished: &Value,
) -> Result<()> {
    // The job must have succeeded, so the progress below covers a
    // full pipeline run rather than an early failure.
    assert_eq!(
        last_finished["state"].as_str(),
        Some("done"),
        "{what} job did not finish clean: {last_finished}"
    );
    // One marker on pull, then the embedding pass's measured fraction;
    // nothing claims a stage the worker cannot see, and nothing repeats
    // the terminal state the closing tick already carries.
    let pairs = stage_kind_pairs(progress);
    let (first, rest) = pairs
        .split_first()
        .with_context(|| format!("{what} job published no worker.progress: {progress:?}"))?;
    assert_eq!(
        first,
        &("processing".to_string(), kind.to_string()),
        "{what} job's first worker.progress is not the pull marker: {progress:?}"
    );
    assert!(
        !rest.is_empty(),
        "{what} job reported no embed progress: {progress:?}"
    );
    assert!(
        rest.iter()
            .all(|pair| pair == &("embed".to_string(), kind.to_string())),
        "{what} job reported something other than embed after the pull marker: {progress:?}"
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
async fn a_rule_job_reports_embed_progress_like_a_book() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let material = tempfile::tempdir()?;
    std::fs::write(material.path().join("01.txt"), BOOK_TEXT)?;
    let rule = material.path().join("rule.toml");
    std::fs::write(
        &rule,
        "schema = 1\n[members]\ninclude = [\"*.txt\"]\n[units]\nby = \"file\"\n",
    )?;

    let (progress, last_finished) = drive_one_job("ingest.submit", json!({ "rule": rule })).await?;
    assert_embed_progress("rule", "book", &progress, &last_finished)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_job_reports_processing_under_its_own_kind() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let paper_dir = tempfile::tempdir()?;
    // The marker is published when the worker pulls the job, before
    // the pipeline opens the source, so a missing file still emits it
    // and the job then fails on its own.
    let paper_path = paper_dir.path().join("absent-paper.pdf");

    let (progress, last_finished) =
        drive_one_job("glean.submit", json!({"paths": [paper_path]})).await?;

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

/// A paper in which no abstract is found has no chunk: the job
/// succeeds and never enters the embedding pass, so the pull marker is
/// all it publishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_job_with_nothing_to_embed_reports_processing_alone() -> Result<()> {
    process_env(ProcessEnv::daemon());
    if !bookrack_extract::pdfium_gate::available() {
        eprintln!("pdfium binary not available; skipping");
        return Ok(());
    }
    let paper_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ops/tests/fixtures/pdf/synthetic_paper_no_abstract.pdf");

    let (progress, last_finished) =
        drive_one_job("glean.submit", json!({"paths": [paper_path]})).await?;

    assert_eq!(
        last_finished["state"].as_str(),
        Some("done"),
        "a paper with no abstract is a clean run: {last_finished}"
    );
    assert_eq!(
        stage_kind_pairs(&progress),
        vec![("processing".to_string(), "paper".to_string())],
        "a paper job with no chunk reported an embedding pass: {progress:?}"
    );
    Ok(())
}
