// SPDX-License-Identifier: Apache-2.0

//! `bookrack queue retry` flips terminal rows back to pending through
//! `queue.retry`, and the worker re-runs them.
//!
//! The queue document is seeded before the daemon starts, so the rows
//! carry controlled ids — a prefix can be exercised against known
//! neighbours — and cover every terminal the verb accepts or refuses
//! without producing them through real ingests. The document is
//! paused, so the rows sit still until the final `queue resume`; the
//! rows name a library the daemon does not serve, so each re-run ends
//! in a book-level failure the test can wait for.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

use std::time::{Duration, Instant};

use bookrack_core::queue::QUEUE_SCHEMA_VERSION;
use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

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

/// Run one client command and return its exit code, stdout and stderr.
async fn client(sandbox: &Sandbox, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::from(bookrack_cmd!(sandbox).build())
        .args(args)
        .output()
        .await
        .expect("run the bookrack client");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

async fn client_json(sandbox: &Sandbox, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let (code, stdout, stderr) = client(sandbox, &full).await;
    assert_eq!(code, Some(0), "{args:?} failed: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{args:?} is not JSON ({e}): {stdout}"))
}

fn job<'a>(listing: &'a Value, id: &str) -> &'a Value {
    listing["jobs"]
        .as_array()
        .and_then(|jobs| jobs.iter().find(|j| j["id"] == id))
        .unwrap_or_else(|| panic!("row {id} missing from {listing}"))
}

#[tokio::test]
async fn retry_reruns_terminal_rows_through_the_command_entry() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();
    std::fs::write(
        sandbox.daemon_state_dir().join("queue.json"),
        serde_json::to_vec(&seeded_document()).expect("serialise the seed"),
    )
    .expect("seed the queue document");

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&lock_path, Duration::from_secs(30)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    // The session lock lands before the daemon serves; the first
    // answered listing is the wait.
    let deadline = Instant::now() + Duration::from_secs(60);
    let listing = loop {
        let (code, stdout, stderr) = client(&sandbox, &["--json", "queue", "list"]).await;
        if code == Some(0) {
            break serde_json::from_str::<Value>(&stdout)
                .unwrap_or_else(|e| panic!("queue list is not JSON ({e}): {stdout}"));
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never answered queue.list: {stderr}",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(listing["paused"], true, "seeded document: {listing}");
    assert_eq!(
        listing["jobs"].as_array().map(Vec::len),
        Some(6),
        "{listing}"
    );

    // No flag: every failed row.
    let reply = client_json(&sandbox, &["queue", "retry"]).await;
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["paused"], true, "{reply}");
    assert_eq!(
        reply["retried"],
        json!([ID_FAILED_A, ID_FAILED_B]),
        "{reply}"
    );

    let pending = client_json(&sandbox, &["queue", "list", "--state", "pending"]).await;
    assert_eq!(
        pending["jobs"].as_array().map(Vec::len),
        Some(3),
        "{pending}"
    );
    for id in [ID_FAILED_A, ID_FAILED_B] {
        let j = job(&pending, id);
        assert_eq!(j["error"], Value::Null, "{j}");
        assert_eq!(j["finished_at"], Value::Null, "{j}");
    }

    // `--state needs-ocr`: the client's spelling reaches the daemon's
    // retryable set, and the anchor pointer goes.
    let reply = client_json(&sandbox, &["queue", "retry", "--state", "needs-ocr"]).await;
    assert_eq!(reply["retried"], json!([ID_NEEDS_OCR]), "{reply}");
    let listing = client_json(&sandbox, &["queue", "list"]).await;
    let j = job(&listing, ID_NEEDS_OCR);
    assert_eq!(j["state"], "pending", "{j}");
    assert_eq!(j["merged_into"], Value::Null, "{j}");

    // Human mode names the count, lists the ids, and says the queue is
    // paused.
    let (code, stdout, stderr) =
        client(&sandbox, &["queue", "retry", "--state", "cancelled"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.first().copied(),
        Some("retried 1 cancelled job"),
        "{stdout}"
    );
    assert_eq!(
        lines.get(1).map(|l| l.trim()),
        Some(ID_CANCELLED),
        "{stdout}"
    );
    assert!(
        lines.last().is_some_and(|l| l.contains("queue is paused")),
        "{stdout}"
    );

    let (code, stdout, stderr) =
        client(&sandbox, &["queue", "retry", "--state", "cancelled"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stdout.starts_with("no cancelled jobs to retry\n"),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .last()
            .is_some_and(|l| l.contains("queue is paused")),
        "{stdout}"
    );

    // Refusals from the daemon: exit 2 with the reason on stderr.
    for (args, needle) in [
        (vec!["queue", "retry", "--job", "0e"], "is done"),
        (vec!["queue", "retry", "--job", ID_PENDING], "is pending"),
        (vec!["queue", "retry", "--job", "0"], "ambiguous"),
        (vec!["queue", "retry", "--job", "zz"], "no job matches"),
    ] {
        let (code, _, stderr) = client(&sandbox, &args).await;
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
    }

    // Refusals from the parser: exit 2 before any RPC is sent.
    for args in [
        vec!["queue", "retry", "--state", "done"],
        vec!["queue", "retry", "--state", "failed", "--job", "0a"],
        vec!["queue", "retry", "--job", ""],
    ] {
        let (code, _, stderr) = client(&sandbox, &args).await;
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("-326"),
            "{args:?} must be refused by the parser, not the daemon: {stderr}"
        );
    }

    // Resume: every flipped row runs again and fails on its unknown
    // library; the done row is untouched.
    let (code, _, stderr) = client(&sandbox, &["queue", "resume"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let settled = loop {
        let listing = client_json(&sandbox, &["queue", "list"]).await;
        let live = listing["summary"]["pending"].as_u64().unwrap_or(u64::MAX)
            + listing["summary"]["running"].as_u64().unwrap_or(u64::MAX);
        if live == 0 {
            break listing;
        }
        assert!(
            Instant::now() < deadline,
            "queue did not drain after resume: {listing}"
        );
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
        assert!(j["finished_at"].is_string(), "{j}");
        assert_ne!(
            j["finished_at"], FINISHED_AT,
            "row {id} was not re-run: {j}"
        );
    }
    let done = job(&settled, ID_DONE);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["finished_at"], FINISHED_AT, "{done}");

    daemon
        .stop(Duration::from_secs(10))
        .await
        .expect("daemon stops on SIGTERM");
}
