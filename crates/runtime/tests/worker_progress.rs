// SPDX-License-Identifier: Apache-2.0

//! End-to-end check of the `worker.progress` emission boundaries: a
//! book job the queue worker carries through the real ingest pipeline
//! emits `extract` when the runner starts and `embed` after it
//! succeeds — exactly the two boundaries `docs/control-plane.md`
//! promises for Phase 2 — and the closing `queue.tick` carries the
//! job's `done` summary.
//!
//! The embedder probe and the embedding pass are both answered by the
//! loopback stub in `common`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

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
/// collecting every `worker.progress` stage published for it.
async fn collect_progress_until_done(
    reader: &mut Reader,
    job_id: &str,
    timeout: Duration,
) -> Result<(Vec<String>, Value)> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut stages = Vec::new();
    loop {
        tokio::select! {
            _ = &mut deadline => {
                return Err(eyre!(
                    "timed out before the job's closing queue.tick; stages so far: {stages:?}"
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
                        stages.push(
                            value["stage"]
                                .as_str()
                                .context("worker.progress without a stage")?
                                .to_string(),
                        );
                    }
                    Some("queue.tick")
                        if value["last_finished"]["job_id"].as_str() == Some(job_id) =>
                    {
                        return Ok((stages, value["last_finished"].clone()));
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successful_ingest_emits_extract_then_embed_progress() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let book_dir = tempfile::tempdir()?;
    let book_path = book_dir.path().join("synthetic-ledger.txt");
    std::fs::write(&book_path, BOOK_TEXT)?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

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
            "method": "ingest.submit",
            "params": {"paths": [book_path]},
        });
        send(&mut wr_w, &submit.to_string()).await?;
        let submit_resp = recv(&mut wr_reader).await?;
        let job_id = submit_resp["result"]["job_ids"][0]
            .as_str()
            .with_context(|| format!("missing job id: {submit_resp}"))?
            .to_string();

        let (stages, last_finished) =
            collect_progress_until_done(&mut obs_reader, &job_id, Duration::from_secs(45)).await?;

        // The job must have succeeded — a failure would also explain
        // an `embed` marker never arriving.
        assert_eq!(
            last_finished["state"].as_str(),
            Some("done"),
            "job did not finish clean: {last_finished}"
        );
        // Exactly the two documented boundaries, in order: `extract`
        // when the runner starts, `embed` after it succeeds.
        assert_eq!(
            stages,
            vec!["extract".to_string(), "embed".to_string()],
            "worker.progress boundaries drifted from the documented pair"
        );

        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut wr_reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
