// SPDX-License-Identifier: Apache-2.0

//! `bookrack queue list --state <s> [--limit N]` selects rows on the
//! client and leaves the rest of the daemon's reply alone.
//!
//! The daemon's `queue.list` knows `limit` but not `state`, so the
//! client has to filter first and cap second: a cap sent to the daemon
//! would truncate the whole queue before the filter saw it. What makes
//! the test discriminate is the queue order — the one cancelled row is
//! older than both failed rows, so a client that lets the daemon cap
//! the queue at one row lists the cancelled job, not a failed one.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required. The failed rows come
//! from source paths that do not exist, which the worker rejects
//! without reaching the embedder.

#![cfg(unix)]

mod common;

use std::time::Duration;

use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

/// Run one client command and return its exit success, stdout and
/// stderr.
async fn client(sandbox: &Sandbox, args: &[&str]) -> (bool, String, String) {
    let output = Command::from(bookrack_cmd!(sandbox).build())
        .args(args)
        .output()
        .await
        .expect("run the bookrack client");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

async fn client_json(sandbox: &Sandbox, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let (ok, stdout, stderr) = client(sandbox, &full).await;
    assert!(ok, "{args:?} failed: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{args:?} is not JSON ({e}): {stdout}"))
}

#[tokio::test]
async fn state_filter_selects_on_the_client_and_keeps_the_summary_whole() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();

    let mut daemon_cmd =
        Command::from(bookrack_cmd!(&sandbox).ollama_url(EmbedStub::url()).build());
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&lock_path, Duration::from_secs(30)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    // The session lock lands before the daemon serves, so the pause
    // doubles as the wait. Pausing first keeps the row cancelled below
    // from being pulled before the cancel reaches it.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (ok, _, stderr) = client(&sandbox, &["queue", "pause"]).await;
        if ok {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never answered queue.pause: {stderr}",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let ghost = |name: &str| sandbox.path().join(name).display().to_string();

    // Oldest row: submitted while paused, then cancelled.
    let submitted = client_json(&sandbox, &["ingest", "--no-wait", &ghost("ghost-0.epub")]).await;
    let cancelled_id = submitted["job_ids"][0]
        .as_str()
        .unwrap_or_else(|| panic!("submit reply carries job_ids: {submitted}"))
        .to_string();
    let (ok, _, stderr) = client(&sandbox, &["queue", "cancel", &cancelled_id]).await;
    assert!(ok, "cancel the paused row: {stderr}");
    let (ok, _, stderr) = client(&sandbox, &["queue", "resume"]).await;
    assert!(ok, "resume the worker: {stderr}");

    // Two younger rows that fail in the worker. The attached ingest
    // returns once its job reaches a terminal state, so after both
    // calls the queue holds exactly the three rows the assertions
    // below count.
    for name in ["ghost-1.epub", "ghost-2.epub"] {
        let _ = client(&sandbox, &["ingest", &ghost(name)]).await;
    }
    let listing = client_json(&sandbox, &["queue", "list"]).await;
    assert_eq!(listing["summary"]["failed"], 2, "fixture queue: {listing}");
    assert_eq!(
        listing["summary"]["cancelled"], 1,
        "fixture queue: {listing}"
    );
    let failed_ids: Vec<&str> = listing["jobs"]
        .as_array()
        .expect("jobs array")
        .iter()
        .filter(|j| j["state"] == "failed")
        .map(|j| j["id"].as_str().expect("job id"))
        .collect();
    assert_eq!(failed_ids.len(), 2, "{listing}");

    // The filtered, capped listing: the oldest *failed* row, not the
    // oldest row; the summary and the envelope untouched.
    let capped = client_json(
        &sandbox,
        &["queue", "list", "--state", "failed", "--limit", "1"],
    )
    .await;
    let jobs = capped["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 1, "one row under --limit 1: {capped}");
    assert_eq!(jobs[0]["state"], "failed", "{capped}");
    assert_eq!(
        jobs[0]["id"], failed_ids[0],
        "the oldest failed row: {capped}"
    );
    assert_eq!(
        capped["summary"], listing["summary"],
        "summary counts the whole queue"
    );
    assert_eq!(capped["paused"], listing["paused"], "{capped}");
    assert_eq!(
        capped["schema_version"], listing["schema_version"],
        "{capped}"
    );

    // Human mode: the failed listing is grouped by cause, with the
    // group's count on its header and no state column.
    let (ok, stdout, stderr) = client(&sandbox, &["queue", "list", "--state", "failed"]).await;
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains("reading the source file failed"),
        "the group header names the cause: {stdout}"
    );
    assert!(
        stdout.contains("(2)"),
        "the header counts the group: {stdout}"
    );
    assert!(
        !stdout.contains("ghost-0.epub"),
        "no row of another state: {stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|l| l.split_whitespace().any(|w| w == "state")),
        "a single-state listing drops the state column: {stdout}"
    );
    assert!(
        !stdout.contains("showing "),
        "an uncapped listing has no cap line: {stdout}"
    );

    let (ok, stdout, _) = client(
        &sandbox,
        &["queue", "list", "--state", "failed", "--limit", "1"],
    )
    .await;
    assert!(ok);
    assert!(
        stdout.contains("(1)"),
        "the header counts the rows shown: {stdout}"
    );
    assert!(
        stdout.contains("showing 1 of 2"),
        "the cap line counts the matches: {stdout}"
    );

    let (ok, stdout, _) = client(&sandbox, &["queue", "list", "--state", "running"]).await;
    assert!(ok);
    assert!(
        stdout.contains("no jobs match state running"),
        "an empty filtered listing names the filter: {stdout}"
    );
    assert!(
        !stdout.contains("queue is empty"),
        "the queue is not empty: {stdout}"
    );

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(10)).await;
}
