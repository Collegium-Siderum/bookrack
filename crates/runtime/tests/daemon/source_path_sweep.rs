// SPDX-License-Identifier: Apache-2.0

//! Every frame the control socket writes after a run of failures is
//! free of a build-machine source marker.
//!
//! A LanceDB error enters the workspace through `bookrack_vectors`
//! alone, and that entry drops the absolute source location the error
//! prints. The queue worker records a failure's text as the error
//! formats it, and `queue.list`, the subscription snapshot, and
//! `queue.tick` serve the recorded text as it is. A dependency error
//! that printed its `file!()` and reached the worker by another route
//! would therefore reach those frames with the build machine's path
//! intact. This file drives one daemon through a source file that does
//! not exist, an embedding backend answering 404, and a LanceDB
//! refusal, keeps every frame the socket wrote — responses, error
//! envelopes, every notification, a fresh snapshot bundle,
//! `events.snapshot`, and `logs.tail` — and checks every string in them
//! for the three markers `redact_source_paths` recognises.
//!
//! The daemon reads its embedder URL from the process environment, so
//! the switch this file flips is the shared stub's and it relies on the
//! per-test process `cargo nextest` runs it in; under a plain
//! `cargo test` the flipped switch reaches every test in the binary.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_test_support::{EmbedFailure, EmbedStub, ProcessEnv, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

/// The markers a dependency source location is recognised by: the
/// cargo registry, a git checkout, and the toolchain's own sources.
const MARKERS: [&str; 3] = ["registry/src/", "git/checkouts/", "/rustc/"];

/// Silence on a subscription that ends its drain.
const QUIET: Duration = Duration::from_secs(1);

/// Every frame the socket wrote, labelled by what asked for it.
struct Sweep {
    frames: Vec<(String, Value)>,
}

impl Sweep {
    fn new() -> Self {
        Sweep { frames: Vec::new() }
    }

    fn keep(&mut self, label: impl Into<String>, frame: Value) {
        self.frames.push((label.into(), frame));
    }

    /// Send one request and keep its response.
    async fn call(
        &mut self,
        reader: &mut Reader,
        w: &mut Writer,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        send(w, &req.to_string()).await?;
        let resp = recv(reader).await?;
        self.keep(format!("{method} response"), resp.clone());
        Ok(resp)
    }

    /// Keep every notification `reader` delivers until it has been
    /// quiet for [`QUIET`].
    async fn drain(&mut self, reader: &mut Reader, label: &str) -> Result<usize> {
        let mut count = 0;
        loop {
            match tokio::time::timeout(QUIET, recv(reader)).await {
                Ok(frame) => {
                    self.keep(format!("{label} #{count}"), frame?);
                    count += 1;
                }
                Err(_) => return Ok(count),
            }
        }
    }

    /// The labelled frames whose strings carry a marker.
    fn leaks(&self) -> Vec<String> {
        self.frames
            .iter()
            .filter(|(_, frame)| {
                strings_of(frame)
                    .into_iter()
                    .any(|text| MARKERS.iter().any(|marker| text.contains(marker)))
            })
            .map(|(label, frame)| format!("{label}: {frame}"))
            .collect()
    }

    /// Whether any kept string contains `needle`.
    fn mentions(&self, needle: &str) -> bool {
        self.frames.iter().any(|(_, frame)| {
            strings_of(frame)
                .into_iter()
                .any(|text| text.contains(needle))
        })
    }
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

/// Submit one path and return its job id.
async fn submit_one(
    sweep: &mut Sweep,
    reader: &mut Reader,
    w: &mut Writer,
    id: u64,
    path: &Path,
) -> Result<String> {
    let resp = sweep
        .call(reader, w, id, "ingest.submit", json!({ "paths": [path] }))
        .await?;
    Ok(resp["result"]["job_ids"][0]
        .as_str()
        .with_context(|| format!("no job id: {resp}"))?
        .to_string())
}

/// Poll `queue.list` until job `id` leaves `pending` / `running`, and
/// return its row.
async fn wait_for(
    sweep: &mut Sweep,
    reader: &mut Reader,
    w: &mut Writer,
    id: &str,
) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        let listed = sweep.call(reader, w, 2, "queue.list", Value::Null).await?;
        let row = listed["result"]["jobs"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(id)))
            .cloned()
            .with_context(|| format!("job {id} is not listed: {listed}"))?;
        match row["state"].as_str() {
            Some("pending" | "running") if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Some("pending" | "running") => return Err(eyre!("the job is still open: {row}")),
            _ => return Ok(row),
        }
    }
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
        let req = json!({ "jsonrpc": "2.0", "id": u64::MAX, "method": "daemon.shutdown" });
        send(&mut w, &req.to_string()).await?;
        let _ = recv(&mut reader).await?;
        result
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_frame_after_a_run_of_failures_carries_a_source_marker() -> Result<()> {
    with_daemon(|sock| async move {
        let mut sweep = Sweep::new();
        let (mut r, mut w) = connect(&sock).await?;
        let (mut sub_r, mut sub_w) = connect(&sock).await?;
        let resp = sweep
            .call(&mut sub_r, &mut sub_w, 1, "events.subscribe", Value::Null)
            .await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");

        let sources = tempfile::tempdir()?;
        let ledger = sources.path().join("synthetic-ledger.txt");
        std::fs::write(
            &ledger,
            "Chapter One\n\n\
             The synthetic ledger recorded every crossing of a fictional ferry, and \
             its keeper added the day's weather beside each entry.\n",
        )?;
        let absent = sources.path().join("absent-almanac.txt");

        // An embedding backend that answers 404 for the model.
        EmbedStub::shared().set_failure(EmbedFailure::ModelNotFound);
        let unembedded = submit_one(&mut sweep, &mut r, &mut w, 3, &ledger).await?;
        let row = wait_for(&mut sweep, &mut r, &mut w, &unembedded).await?;
        assert_eq!(row["state"], "failed", "{row}");
        assert!(
            row["problem"]["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("ollama pull")),
            "the backend's refusal is the recorded failure: {row}"
        );
        EmbedStub::shared().set_failure(EmbedFailure::None);

        // A source file that does not exist.
        let unread = submit_one(&mut sweep, &mut r, &mut w, 4, &absent).await?;
        let row = wait_for(&mut sweep, &mut r, &mut w, &unread).await?;
        assert_eq!(row["state"], "failed", "{row}");
        assert!(
            row["error"]
                .as_str()
                .is_some_and(|error| error.contains("reading the source file failed")),
            "the missing file is the recorded failure: {row}"
        );

        // A LanceDB refusal raised through a public method over a
        // library with content in it.
        let stored = submit_one(&mut sweep, &mut r, &mut w, 5, &ledger).await?;
        let row = wait_for(&mut sweep, &mut r, &mut w, &stored).await?;
        assert_eq!(row["state"], "done", "{row}");
        let refused = sweep
            .call(
                &mut r,
                &mut w,
                6,
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
            "a source location was there to redact: {refused}"
        );

        // Everything else the socket has to say.
        let listed = sweep
            .call(&mut r, &mut w, 7, "queue.list", Value::Null)
            .await?;
        let failed = listed["result"]["jobs"]
            .as_array()
            .map(|rows| rows.iter().filter(|row| row["state"] == "failed").count())
            .unwrap_or(0);
        assert_eq!(failed, 2, "both failures are listed: {listed}");
        sweep
            .call(&mut r, &mut w, 8, "logs.tail", json!({ "n": 1000 }))
            .await?;
        sweep
            .call(
                &mut r,
                &mut w,
                9,
                "events.snapshot",
                json!({ "channels": ["queue.list", "queue.tick", "daemon.state"] }),
            )
            .await?;
        let streamed = sweep.drain(&mut sub_r, "notification").await?;
        assert!(
            streamed > 0,
            "the subscription saw the run: {} frames kept",
            sweep.frames.len()
        );
        let (mut late_r, mut late_w) = connect(&sock).await?;
        let resp = sweep
            .call(
                &mut late_r,
                &mut late_w,
                10,
                "events.subscribe",
                Value::Null,
            )
            .await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");
        let bundled = sweep.drain(&mut late_r, "snapshot").await?;
        assert!(bundled > 0, "a fresh subscription opens with its snapshot");

        assert!(
            sweep.mentions("reading the source file failed")
                && sweep.mentions("Refine factor cannot be zero"),
            "the kept frames carry the failures they are checked for"
        );
        let leaks = sweep.leaks();
        assert!(
            leaks.is_empty(),
            "{} frame(s) carry a build-machine source marker:\n{}",
            leaks.len(),
            leaks.join("\n")
        );
        Ok(())
    })
    .await
}
