// SPDX-License-Identifier: Apache-2.0

//! `ingest.submit` in its rule form: a rule file that can run is queued
//! as one job and carried to `done`; a submission that cannot run, or
//! that mixes the rule form with the file form, is refused as invalid
//! params with a next step and leaves the queue as it was.
//!
//! The queue is paused while the refusals are sent, so the one job the
//! accepted submission wrote stays pending and the row count is exact.
//! The embedder is the loopback stub in `common`.

#![cfg(unix)]

use std::time::Duration;

use eyre::{Result, bail};
use serde_json::{Value, json};

use crate::common::{build_opts, connect, join_with_deadline, recv, send};
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

const CHAPTER: &str = "\
The synthetic narrator opened the synthetic ledger and began to
count. Every entry in the ledger described a fictional object, and
every fictional object had a fictional weight. Counting the weights
took the narrator most of the morning.
";

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
async fn a_rule_submission_is_checked_then_queued_as_one_rule_job() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let material = tempfile::tempdir()?;
    std::fs::write(material.path().join("01.txt"), CHAPTER)?;
    std::fs::write(
        material.path().join("02.txt"),
        CHAPTER.replace("ledger", "atlas"),
    )?;
    let rule = material.path().join("rule.toml");
    std::fs::write(&rule, RULE)?;
    let broken = material.path().join("broken.toml");
    std::fs::write(&broken, format!("{RULE}\nstray = 1\n"))?;
    let empty = tempfile::tempdir()?;
    let memberless = empty.path().join("rule.toml");
    std::fs::write(&memberless, RULE)?;
    // Any file serves as a source nothing was registered from.
    let scan = material.path().join("scan.pdf");
    std::fs::write(&scan, b"%PDF-1.4 synthetic")?;
    let absent = material.path().join("absent.pdf");

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
        let paused = call(&mut reader, &mut w, "queue.pause", json!({})).await?;
        assert_eq!(paused["result"]["paused"], true, "{paused}");

        let accepted = call(
            &mut reader,
            &mut w,
            "ingest.submit",
            json!({ "rule": rule, "priority": "high" }),
        )
        .await?;
        let ids = accepted["result"]["job_ids"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(ids.len(), 1, "one rule, one job: {accepted}");

        // (label, params, a fragment of the summary, whether a hint is owed)
        let refusals: Vec<(&str, Value, &str, bool)> = vec![
            (
                "an unknown key",
                json!({ "rule": broken }),
                "cannot read rule file",
                true,
            ),
            (
                "no member",
                json!({ "rule": memberless }),
                "matches no member",
                true,
            ),
            (
                "a rule beside paths",
                json!({ "rule": rule, "paths": ["/tmp/book.epub"] }),
                "rule",
                true,
            ),
            (
                "a rule held for metadata",
                json!({ "rule": rule, "hold_for_metadata": true }),
                "is no longer accepted",
                true,
            ),
            (
                "a rule walked recursively",
                json!({ "rule": rule, "recursive": true }),
                "recursive",
                true,
            ),
            (
                "a partial product of no source",
                json!({ "rule": rule, "allow_partial": true }),
                "allow_partial",
                true,
            ),
            (
                "a source beside paths",
                json!({ "paths": ["/tmp/book.epub"], "source": scan }),
                "source",
                true,
            ),
            (
                "a relative rule path",
                json!({ "rule": "rule.toml" }),
                "rule",
                true,
            ),
            (
                "a source nothing was registered from",
                json!({ "rule": rule, "source": scan }),
                "is not waiting for OCR",
                true,
            ),
            (
                "a source that is not a file",
                json!({ "rule": rule, "source": absent }),
                "is not a file",
                true,
            ),
            ("neither form", json!({}), "paths", false),
        ];
        for (label, params, fragment, hinted) in refusals {
            let refused = call(&mut reader, &mut w, "ingest.submit", params).await?;
            assert_eq!(refused["error"]["code"], -32602, "{label}: {refused}");
            let message = refused["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains(fragment), "{label}: {refused}");
            if hinted {
                assert!(
                    refused["error"]["data"]["hint"].is_string(),
                    "{label} names a next step: {refused}"
                );
            }
        }
        let broken_reply = call(
            &mut reader,
            &mut w,
            "ingest.submit",
            json!({ "rule": broken }),
        )
        .await?;
        assert!(
            broken_reply["error"]["data"]["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("stray")),
            "the refusal names the key at fault: {broken_reply}"
        );

        let unknown = call(
            &mut reader,
            &mut w,
            "ingest.submit",
            json!({ "rule": rule, "library": "no-such-library" }),
        )
        .await?;
        assert_eq!(unknown["error"]["code"], -32010, "{unknown}");

        let listed = call(&mut reader, &mut w, "queue.list", json!({})).await?;
        let jobs = listed["result"]["jobs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(jobs.len(), 1, "no refusal wrote a row: {listed}");
        let job = &jobs[0];
        assert_eq!(job["id"], ids[0], "{job}");
        assert_eq!(job["kind"], "book", "{job}");
        assert_eq!(job["priority"], "high", "{job}");
        assert_eq!(job["op"]["type"], "ingest", "{job}");
        assert_eq!(job["op"]["path"], json!(rule), "{job}");
        assert!(job["op"].get("hold_for_metadata").is_none(), "{job}");
        assert_eq!(
            job["op"]["input"],
            json!({ "type": "rule", "source_pdf": null, "allow_partial": false }),
            "{job}"
        );

        let resumed = call(&mut reader, &mut w, "queue.resume", json!({})).await?;
        assert!(resumed.get("error").is_none(), "{resumed}");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            let listed = call(&mut reader, &mut w, "queue.list", json!({})).await?;
            let state = listed["result"]["jobs"][0]["state"]
                .as_str()
                .unwrap_or_default();
            match state {
                "done" => break,
                "pending" | "running" => {}
                _ => bail!("the rule job did not finish as done: {listed}"),
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("the rule job never finished: {listed}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let _ = call(&mut reader, &mut w, "daemon.shutdown", json!({})).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}
