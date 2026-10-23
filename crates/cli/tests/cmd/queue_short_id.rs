// SPDX-License-Identifier: Apache-2.0

//! The id column `queue list` prints locates the job it stands for.
//!
//! A UUIDv7 leads with its millisecond timestamp, so every job queued
//! within the same minute or so shares the leading characters of its
//! id. The test submits a directory of two books in one call — the two
//! ids are generated microseconds apart — then copies each row's id
//! cell out of the human listing and feeds it back to `queue cancel`
//! and `queue retry --job`. Each must resolve to its own job, not be
//! refused as ambiguous.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required. The queue is paused
//! before the submission, so the rows stay put.

#![cfg(unix)]

use std::time::{Duration, Instant};

use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

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

/// The id cell of the human listing's row for `file`: the row's first
/// cell inside the table's outer border.
fn id_cell<'a>(listing: &'a str, file: &str) -> &'a str {
    let cells = |line: &'a str| line.split_whitespace().filter(|cell| *cell != "\u{2502}");
    listing
        .lines()
        .find(|line| cells(line).any(|cell| cell == file))
        .and_then(|line| cells(line).next())
        .unwrap_or_else(|| panic!("no row for {file} in:\n{listing}"))
}

#[tokio::test]
async fn the_listed_id_locates_its_job() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();
    let batch = sandbox.path().join("batch");
    std::fs::create_dir_all(&batch).expect("create the batch directory");
    for name in ["alpha.epub", "beta.epub"] {
        std::fs::write(batch.join(name), b"").expect("create a source file");
    }

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

    // The session lock lands before the daemon serves, so the pause
    // doubles as the wait.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (code, _, stderr) = client(&sandbox, &["queue", "pause"]).await;
        if code == Some(0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never answered queue.pause: {stderr}",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let batch_arg = batch.display().to_string();
    let submitted = client_json(
        &sandbox,
        &["ingest", "--no-wait", "--recursive", &batch_arg],
    )
    .await;
    let ids: Vec<&str> = submitted["job_ids"]
        .as_array()
        .unwrap_or_else(|| panic!("submit reply carries job_ids: {submitted}"))
        .iter()
        .map(|v| v.as_str().expect("job id"))
        .collect();
    assert_eq!(ids.len(), 2, "{submitted}");
    assert_eq!(
        ids[0][..8],
        ids[1][..8],
        "two ids generated in one submission share their timestamp head; \
         the test relies on it: {ids:?}"
    );

    let listing = client_json(&sandbox, &["queue", "list"]).await;
    let full_id = |file: &str| -> String {
        listing["jobs"]
            .as_array()
            .and_then(|jobs| {
                jobs.iter()
                    .find(|j| j["path"].as_str().is_some_and(|p| p.ends_with(file)))
            })
            .and_then(|j| j["id"].as_str())
            .unwrap_or_else(|| panic!("no row for {file}: {listing}"))
            .to_string()
    };
    let alpha_id = full_id("alpha.epub");
    let beta_id = full_id("beta.epub");

    let (code, human, stderr) = client(&sandbox, &["queue", "list"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    let alpha_cell = id_cell(&human, "alpha.epub").to_string();
    let beta_cell = id_cell(&human, "beta.epub").to_string();
    assert_ne!(
        alpha_cell, beta_cell,
        "two rows queued together print different ids:\n{human}"
    );

    // The printed cell, pasted back, cancels its own row.
    let (code, stdout, stderr) =
        client(&sandbox, &["--json", "queue", "cancel", &alpha_cell]).await;
    assert_eq!(code, Some(0), "queue cancel {alpha_cell}: {stderr}");
    let reply: Value = serde_json::from_str(&stdout).expect("cancel reply is JSON");
    assert_eq!(reply["job_id"], alpha_id.as_str(), "{reply}");

    // And retries it.
    let reply = client_json(&sandbox, &["queue", "retry", "--job", &alpha_cell]).await;
    assert_eq!(reply["retried"], json!([alpha_id]), "{reply}");

    // The other row is untouched, and its own cell reaches it.
    let (code, stdout, stderr) = client(&sandbox, &["--json", "queue", "cancel", &beta_cell]).await;
    assert_eq!(code, Some(0), "queue cancel {beta_cell}: {stderr}");
    let reply: Value = serde_json::from_str(&stdout).expect("cancel reply is JSON");
    assert_eq!(reply["job_id"], beta_id.as_str(), "{reply}");

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(10)).await;
}
