// SPDX-License-Identifier: Apache-2.0

//! A submission the caller does not wait on is reported in the output
//! mode asked for: one human line naming what was queued, the daemon's
//! reply under `--json`, nothing under `--quiet`.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required. The queue is paused
//! before the submissions, so the rows stay put.

#![cfg(unix)]

use std::time::{Duration, Instant};

use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

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

/// The id of the one queue row whose path ends in `file`.
fn job_id_for(listing: &Value, file: &str) -> String {
    let rows: Vec<&Value> = listing["jobs"]
        .as_array()
        .unwrap_or_else(|| panic!("queue list carries jobs: {listing}"))
        .iter()
        .filter(|job| job["path"].as_str().is_some_and(|p| p.ends_with(file)))
        .collect();
    assert_eq!(rows.len(), 1, "one row for {file}: {listing}");
    rows[0]["id"].as_str().expect("job id").to_string()
}

#[tokio::test]
async fn a_submission_not_waited_on_is_reported_in_the_mode_asked_for() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();
    let alpha = sandbox.path().join("alpha.epub");
    let paper = sandbox.path().join("paper.epub");
    let batch = sandbox.path().join("batch");
    std::fs::create_dir_all(&batch).expect("create the batch directory");
    for path in [
        &alpha,
        &paper,
        &batch.join("beta.epub"),
        &batch.join("gamma.epub"),
    ] {
        std::fs::write(path, b"").expect("create a source file");
    }
    let alpha = alpha.display().to_string();
    let paper = paper.display().to_string();
    let batch = batch.display().to_string();

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(30))
        .await
        .expect("bookrack run did not become ready");

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

    // One job: the file and the id that reaches its row, on one line.
    let (code, stdout, stderr) = client(&sandbox, &["ingest", "--no-wait", &alpha]).await;
    assert_eq!(code, Some(0), "{stderr}");
    let printed = stdout
        .strip_prefix("Queued alpha.epub as ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .filter(|id| !id.is_empty() && !id.contains(char::is_whitespace))
        .unwrap_or_else(|| panic!("one line naming the file and its job: {stdout:?}"));
    let listing = client_json(&sandbox, &["queue", "list"]).await;
    let alpha_id = job_id_for(&listing, "alpha.epub");
    let reply = client_json(&sandbox, &["queue", "cancel", printed]).await;
    assert_eq!(
        reply["job_id"],
        alpha_id.as_str(),
        "the printed id reaches its job: {reply}"
    );

    // Several jobs: counted, under the name that was submitted.
    let (code, stdout, stderr) =
        client(&sandbox, &["ingest", "--no-wait", "--recursive", &batch]).await;
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "Queued 2 jobs for batch\n");

    // The paper side says the same thing.
    let (code, stdout, stderr) = client(&sandbox, &["glean", "--no-wait", &paper]).await;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stdout.starts_with("Queued paper.epub as ") && stdout.lines().count() == 1,
        "{stdout:?}"
    );

    // `--json` keeps the daemon's reply.
    let reply = client_json(&sandbox, &["ingest", "--no-wait", &alpha]).await;
    assert_eq!(
        reply["job_ids"].as_array().map(Vec::len),
        Some(1),
        "{reply}"
    );

    // `--quiet` prints nothing.
    let (code, stdout, stderr) =
        client(&sandbox, &["--quiet", "ingest", "--no-wait", &alpha]).await;
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "");

    daemon
        .stop(Duration::from_secs(10))
        .await
        .expect("daemon stops on SIGTERM");
}
