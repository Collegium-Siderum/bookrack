// SPDX-License-Identifier: Apache-2.0

//! Integration test for the `queue.retry` control-plane method.
//!
//! The queue document is written into the daemon state directory
//! before bring-up, so the rows carry controlled ids — a prefix can be
//! exercised against known neighbours — and cover every terminal the
//! method accepts or refuses without producing them through real
//! ingests. The document is paused, so the rows sit still until the
//! final `queue.resume`, after which the worker is expected to re-run
//! every row the retries flipped: a row that names a library the
//! daemon does not serve fails as a book-level outcome, which is a
//! terminal the test can wait for.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use eyre::{Context, Result, eyre};
use serde_json::{Value, json};

use crate::common::{await_channel, build_opts, connect, join_with_deadline, recv, send};
use bookrack_runtime::queue::QUEUE_SCHEMA_VERSION;
use bookrack_test_support::{ProcessEnv, process_env};

const ID_FAILED_A: &str = "0a000000-0000-7000-8000-000000000001";
const ID_FAILED_B: &str = "0b000000-0000-7000-8000-000000000002";
const ID_NEEDS_OCR: &str = "0c000000-0000-7000-8000-000000000003";
const ID_CANCELLED: &str = "0d000000-0000-7000-8000-000000000004";
const ID_DONE: &str = "0e000000-0000-7000-8000-000000000005";
const ID_PENDING: &str = "0f000000-0000-7000-8000-000000000006";

const QUEUED_AT: &str = "2026-01-02T03:04:05Z";
const FINISHED_AT: &str = "2026-01-02T03:05:05Z";
const FAILURE_TEXT: &str =
    "ingest: registry-mediated ingest: reading the source file failed: no such file";

fn row(id: &str, state: &str, error: Option<&str>, merged_into: Option<i64>) -> Value {
    let terminal = state != "pending";
    json!({
        "id": id,
        "library": "nowhere",
        "path": format!("/nonexistent/{id}.epub"),
        "priority": "normal",
        "force": false,
        "hold_for_metadata": false,
        "kind": "book",
        "intake_ocr": null,
        "audit_profile": null,
        "state": state,
        "queued_at": QUEUED_AT,
        "started_at": if terminal { Value::String(QUEUED_AT.into()) } else { Value::Null },
        "finished_at": if terminal { Value::String(FINISHED_AT.into()) } else { Value::Null },
        "error": error,
        "merged_into": merged_into,
    })
}

fn seeded_document() -> Value {
    json!({
        "schema_version": QUEUE_SCHEMA_VERSION,
        "paused": true,
        "jobs": [
            row(ID_FAILED_A, "failed", Some(FAILURE_TEXT), None),
            row(ID_FAILED_B, "failed", Some(FAILURE_TEXT), None),
            row(ID_NEEDS_OCR, "needs_ocr", None, Some(42)),
            row(ID_CANCELLED, "cancelled", None, None),
            row(ID_DONE, "done", None, None),
            row(ID_PENDING, "pending", None, None),
        ],
    })
}

fn job<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["result"]["jobs"]
        .as_array()
        .and_then(|jobs| jobs.iter().find(|j| j["id"] == id))
        .unwrap_or_else(|| panic!("row {id} missing from {list}"))
}

fn ids(resp: &Value) -> Vec<String> {
    resp["result"]["retried"]
        .as_array()
        .unwrap_or_else(|| panic!("retried must be an array: {resp}"))
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_flips_terminal_rows_in_place_and_the_worker_reruns_them() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    std::fs::write(
        sandbox.daemon_state_dir().join("queue.json"),
        serde_json::to_vec(&seeded_document())?,
    )?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut obs_reader, mut obs_w) = connect(&sock).await?;
        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#,
        )
        .await?;
        let resp = recv(&mut obs_reader).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");
        // Drain the snapshot bundle: its queue.tick reads the seeded
        // document (pending == 1), not the state after a retry. The
        // worker's own start-up tick carries the same count and may
        // land before or after this point, so the tick awaited below
        // is matched on its value rather than on frame position.
        for _ in 0..7 {
            let _ = recv(&mut obs_reader).await?;
        }

        let (mut reader, mut writer) = connect(&sock).await?;
        let mut next_id = 10u64;
        let mut call = |method: &str, params: Value| {
            next_id += 1;
            let frame = json!({
                "jsonrpc": "2.0",
                "id": next_id,
                "method": method,
                "params": params,
            });
            frame.to_string()
        };

        // The seeded document is what the daemon serves.
        send(&mut writer, &call("queue.list", Value::Null)).await?;
        let list = recv(&mut reader).await?;
        assert_eq!(list["result"]["paused"], Value::Bool(true), "{list}");
        assert_eq!(
            list["result"]["jobs"].as_array().map(Vec::len),
            Some(6),
            "{list}"
        );

        // Every failed row flips; the tick that follows counts them.
        send(
            &mut writer,
            &call("queue.retry", json!({ "state": "failed" })),
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["result"]["ok"], Value::Bool(true), "{resp}");
        assert_eq!(resp["result"]["paused"], Value::Bool(true), "{resp}");
        assert_eq!(
            ids(&resp),
            vec![ID_FAILED_A.to_string(), ID_FAILED_B.to_string()]
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let tick = await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(5))
                .await
                .context("queue.tick after queue.retry")?;
            if tick["params"]["value"]["pending"] == json!(3) {
                break;
            }
            assert_eq!(
                tick["params"]["value"]["pending"],
                json!(1),
                "a tick between the seeded state and the retry: {tick}"
            );
            assert!(
                Instant::now() < deadline,
                "no tick counted the retried rows: {tick}"
            );
        }

        send(&mut writer, &call("queue.list", Value::Null)).await?;
        let list = recv(&mut reader).await?;
        for id in [ID_FAILED_A, ID_FAILED_B] {
            let j = job(&list, id);
            assert_eq!(j["state"], "pending", "{j}");
            for key in ["error", "finished_at", "started_at", "merged_into"] {
                assert_eq!(j[key], Value::Null, "{key} must be cleared: {j}");
            }
            assert_eq!(j["queued_at"], QUEUED_AT, "{j}");
        }
        let control = job(&list, ID_NEEDS_OCR);
        assert_eq!(control["merged_into"], json!(42), "{control}");
        assert_eq!(control["state"], "needs_ocr", "{control}");

        // needs_ocr is retryable and loses its anchor pointer.
        send(
            &mut writer,
            &call("queue.retry", json!({ "state": "needs_ocr" })),
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(ids(&resp), vec![ID_NEEDS_OCR.to_string()], "{resp}");
        send(&mut writer, &call("queue.list", Value::Null)).await?;
        let list = recv(&mut reader).await?;
        let j = job(&list, ID_NEEDS_OCR);
        assert_eq!(j["state"], "pending", "{j}");
        assert_eq!(j["merged_into"], Value::Null, "{j}");
        assert_eq!(j["finished_at"], Value::Null, "{j}");

        // A prefix that resolves to one retryable row.
        send(&mut writer, &call("queue.retry", json!({ "job_id": "0d" }))).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(ids(&resp), vec![ID_CANCELLED.to_string()], "{resp}");

        // Refusals, each with its own code and wording.
        let refusals: [(Value, i64, &str); 8] = [
            (json!({ "job_id": "0e" }), -32602, "is done"),
            (json!({ "job_id": ID_PENDING }), -32602, "is pending"),
            (json!({ "job_id": "0" }), -32011, "ambiguous"),
            (json!({ "job_id": "zz" }), -32011, "no job matches"),
            (json!({ "state": "done" }), -32602, "not retryable"),
            (
                json!({ "state": "bogus" }),
                -32602,
                "invalid queue.retry params",
            ),
            (json!({}), -32602, "exactly one"),
            (
                json!({ "state": "failed", "job_id": "0a" }),
                -32602,
                "exactly one",
            ),
        ];
        for (params, code, needle) in refusals {
            send(&mut writer, &call("queue.retry", params.clone())).await?;
            let resp = recv(&mut reader).await?;
            assert_eq!(resp["error"]["code"], json!(code), "{params}: {resp}");
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains(needle), "{params}: {resp}");
        }

        // Nothing is left in `cancelled`, and an empty selection is not
        // an error.
        send(
            &mut writer,
            &call("queue.retry", json!({ "state": "cancelled" })),
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["result"]["ok"], Value::Bool(true), "{resp}");
        assert!(ids(&resp).is_empty(), "{resp}");

        // Resume: the worker re-runs every flipped row as a fresh
        // submission, and each one fails on its unknown library.
        let resumed_at = Utc::now();
        send(&mut writer, &call("queue.resume", Value::Null)).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["result"]["paused"], Value::Bool(false), "{resp}");

        let deadline = Instant::now() + Duration::from_secs(30);
        let settled = loop {
            send(&mut writer, &call("queue.list", Value::Null)).await?;
            let list = recv(&mut reader).await?;
            let live = list["result"]["jobs"]
                .as_array()
                .map(|jobs| {
                    jobs.iter()
                        .filter(|j| j["state"] == "pending" || j["state"] == "running")
                        .count()
                })
                .unwrap_or(usize::MAX);
            if live == 0 {
                break list;
            }
            if Instant::now() > deadline {
                return Err(eyre!("queue did not drain after resume: {list}"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        for id in [
            ID_FAILED_A,
            ID_FAILED_B,
            ID_NEEDS_OCR,
            ID_CANCELLED,
            ID_PENDING,
        ] {
            let j = job(&settled, id);
            assert_eq!(j["state"], "failed", "{j}");
            let finished: DateTime<Utc> = j["finished_at"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| panic!("finished_at must be a timestamp: {j}"));
            assert!(finished > resumed_at, "row {id} was not re-run: {j}");
        }
        let done = job(&settled, ID_DONE);
        assert_eq!(done["state"], "done", "{done}");
        assert_eq!(done["finished_at"], FINISHED_AT, "{done}");

        send(&mut writer, &call("daemon.shutdown", Value::Null)).await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
