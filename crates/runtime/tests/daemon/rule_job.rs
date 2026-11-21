// SPDX-License-Identifier: Apache-2.0

//! A queued ingest whose input is a rule file, carried by the queue
//! worker through the real book pipeline: the first job writes the
//! book and lights up the read path of a root nothing had been
//! ingested into, an identical second job is skipped as a duplicate of
//! it, and a paper-side job naming a rule is failed rather than handed
//! to the paper pipeline.
//!
//! The jobs are seeded into the queue document, paused, before the
//! daemon starts, and released once the test is connected. The
//! embedder is the loopback stub in `common`.

#![cfg(unix)]

use std::time::Duration;

use eyre::{Result, bail};
use serde_json::{Value, json};

use crate::common::{build_opts, connect, join_with_deadline, recv, send};
use bookrack_core::queue::QUEUE_SCHEMA_VERSION;
use bookrack_test_support::{ProcessEnv, process_env};

const RULE: &str = "\
schema = 1

[members]
include = [\"*.txt\"]

[units]
by = \"file\"

[structure]
unit_titles = \"none\"
headings = \"none\"

[biblio]
title = \"A Field Guide\"
language = \"en\"
";

const CHAPTER_ONE: &str = "\
The synthetic narrator opened the synthetic ledger and began to
count. Every entry in the ledger described a fictional object, and
every fictional object had a fictional weight. Counting the weights
took the narrator most of the morning.

By noon the ledger was balanced. The narrator celebrated with a
fictional meal and recorded the celebration as a footnote.
";

const CHAPTER_TWO: &str = "\
On the second day the ledger grew a second column. The new column
held colours instead of weights, and the narrator sorted them from
dull to bright. Sorting colours was slower than counting weights.

When the sorting ended the narrator closed the ledger, satisfied
that both columns agreed with each other in every fictional respect.
";

const ID_FIRST: &str = "01890000-0000-7000-8000-000000000001";
const ID_AGAIN: &str = "01890000-0000-7000-8000-000000000002";
const ID_PAPER: &str = "01890000-0000-7000-8000-000000000003";

fn rule_row(id: &str, kind: &str, rule: &std::path::Path) -> Value {
    json!({
        "id": id,
        "library": "",
        "kind": kind,
        "priority": "normal",
        "op": {
            "type": "ingest",
            "path": rule,
            "force": false,
            "audit_profile": null,
            "input": { "type": "rule", "source_pdf": null, "allow_partial": false },
        },
        "intake_ocr": null,
        "state": "pending",
        "queued_at": "2026-01-02T03:04:05Z",
        "started_at": null,
        "finished_at": null,
        "error": null,
        "problem": null,
        "merged_into": null,
    })
}

async fn call(
    reader: &mut crate::common::Reader,
    writer: &mut crate::common::Writer,
    method: &str,
    params: Value,
) -> Result<Value> {
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    send(writer, &request.to_string()).await?;
    recv(reader).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_worker_carries_a_rule_job_through_the_book_pipeline() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let state_dir = std::path::PathBuf::from(std::env::var("BOOKRACK_DAEMON_STATE_DIR")?);
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let material = tempfile::tempdir()?;
    std::fs::write(material.path().join("01.txt"), CHAPTER_ONE)?;
    std::fs::write(material.path().join("02.txt"), CHAPTER_TWO)?;
    let rule = material.path().join("rule.toml");
    std::fs::write(&rule, RULE)?;

    let seeded = json!({
        "schema_version": QUEUE_SCHEMA_VERSION,
        "paused": true,
        "jobs": [
            rule_row(ID_FIRST, "book", &rule),
            rule_row(ID_AGAIN, "book", &rule),
            rule_row(ID_PAPER, "paper", &rule),
        ],
    });
    std::fs::create_dir_all(&state_dir)?;
    std::fs::write(state_dir.join("queue.json"), seeded.to_string())?;

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
        let resumed = call(&mut reader, &mut w, "queue.resume", json!({})).await?;
        assert!(resumed.get("error").is_none(), "{resumed}");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        let jobs = loop {
            let listed = call(&mut reader, &mut w, "queue.list", json!({})).await?;
            let jobs = listed["result"]["jobs"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let open = jobs
                .iter()
                .any(|job| matches!(job["state"].as_str(), Some("pending" | "running")));
            if jobs.len() == 3 && !open {
                break jobs;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("the rule jobs never finished: {listed}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let job = |id: &str| {
            jobs.iter()
                .find(|job| job["id"] == id)
                .unwrap_or_else(|| panic!("job {id} is not listed: {jobs:?}"))
        };

        let first = job(ID_FIRST);
        assert_eq!(first["state"], "done", "{first}");
        assert_eq!(first["merged_into"], Value::Null, "{first}");
        assert_eq!(first["op"]["input"]["type"], "rule", "{first}");

        let again = job(ID_AGAIN);
        assert_eq!(again["state"], "skipped_duplicate", "{again}");
        assert!(
            again["merged_into"].is_i64(),
            "a skipped rule job points at the intake it duplicates: {again}"
        );

        let paper = job(ID_PAPER);
        assert_eq!(paper["state"], "failed", "{paper}");
        let error = paper["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("rule") && error.contains("paper"),
            "the failure says a rule is not a paper source: {paper}"
        );

        let stats = call(&mut reader, &mut w, "library.stats", json!({})).await?;
        assert_eq!(
            stats["result"]["intake_count_by_format"]["rule-text"], 1,
            "the rule-built book is counted under its format: {stats}"
        );
        assert_eq!(
            stats["result"]["papers"]["intake_counts_by_status"]
                .as_object()
                .map(|counts| counts.values().filter_map(Value::as_u64).sum::<u64>())
                .unwrap_or(0),
            0,
            "the paper-side job wrote nothing: {stats}"
        );

        // The root held no vector store when the daemon bound its read
        // path; the book the first rule ingest wrote is searchable in
        // the same process.
        let found = call(
            &mut reader,
            &mut w,
            "library.search",
            json!({ "query": "the narrator sorted colours", "top_k": 5 }),
        )
        .await?;
        let hits = found["result"].as_array().cloned().unwrap_or_default();
        assert!(
            !hits.is_empty(),
            "search finds the rule-built book: {found}"
        );

        let _ = call(&mut reader, &mut w, "daemon.shutdown", json!({})).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}
