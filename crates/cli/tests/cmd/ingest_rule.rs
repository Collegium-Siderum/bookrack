// SPDX-License-Identifier: Apache-2.0

//! `bookrack ingest --rule`: the preview runs in the client with no
//! daemon and no library, a rule that cannot run is refused there in
//! the three parts a submission's refusal carries, and a submission
//! reaches the daemon as one rule job under absolute paths.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`]. The queue is paused before the submission, so the
//! row stays put.

#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, Spawn, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

/// The name the sandbox's data root is registered under.
const LIBRARY: &str = "main";

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
every fictional object had a fictional weight.
";

/// Two chapters and the rule over them in `dir`; a second rule beside
/// them that carries a key no rule has.
fn lay_out(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create the material directory");
    std::fs::write(dir.join("01.txt"), CHAPTER).expect("chapter one");
    std::fs::write(dir.join("02.txt"), CHAPTER.replace("ledger", "atlas")).expect("chapter two");
    std::fs::write(dir.join("rule.toml"), RULE).expect("rule");
    std::fs::write(dir.join("broken.toml"), format!("{RULE}\nstray = 1\n")).expect("broken rule");
}

async fn run(spawn: Spawn, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::from(spawn.build())
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

#[tokio::test]
async fn a_rule_preview_needs_no_daemon_and_no_library() {
    let sandbox = Sandbox::new();
    sandbox.name_data_dir(LIBRARY);
    let material = sandbox.path().join("material");
    lay_out(&material);
    let rule = material.join("rule.toml").display().to_string();
    let broken = material.join("broken.toml").display().to_string();
    let args = ["ingest", "--rule", rule.as_str(), "--dry-run"];

    // A selected library with no overlay directory reads the shipped
    // rules, and the report says so.
    let (code, stdout, stderr) = run(bookrack_cmd!(&sandbox).library(LIBRARY), &args).await;
    assert_eq!(code, Some(0), "{stdout} {stderr}");
    assert!(stdout.contains("units: 2 (2 numbered, 1 to 2)"), "{stdout}");
    assert!(stdout.contains("audit rules: shipped defaults"), "{stdout}");

    // Once the library carries one, the report names it.
    let overlay = sandbox.data_dir().join("audit-rules");
    std::fs::create_dir_all(&overlay).expect("create the overlay directory");
    let (code, stdout, stderr) = run(bookrack_cmd!(&sandbox).library(LIBRARY), &args).await;
    assert_eq!(code, Some(0), "{stdout} {stderr}");
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("audit rules: ") && line.ends_with("audit-rules")),
        "the report says whose audit rules it read: {stdout}"
    );

    // With nothing selected anywhere, the shipped rules stand in.
    let nowhere = || bookrack_cmd!(&sandbox).without_registry();
    let (code, stdout, stderr) = run(nowhere(), &args).await;
    assert_eq!(code, Some(0), "{stdout} {stderr}");
    assert!(
        stdout.contains("audit rules: shipped defaults"),
        "the report says no library was read: {stdout}"
    );

    let (code, stdout, stderr) = run(
        nowhere(),
        &["--json", "ingest", "--rule", rule.as_str(), "--dry-run"],
    )
    .await;
    assert_eq!(code, Some(0), "{stdout} {stderr}");
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("the preview is not JSON ({e}): {stdout}"));
    assert_eq!(report["units"], 2, "{report}");
    assert_eq!(report["completeness"]["numbered"], 2, "{report}");
    assert_eq!(report["book"]["adapter"], "rule-text", "{report}");
    assert_eq!(report["audit_rules"], Value::Null, "{report}");

    // An explicit selection that resolves nowhere is refused, not
    // replaced by the shipped rules.
    let (code, stdout, stderr) = run(
        nowhere(),
        &[
            "--library",
            "no-such-library",
            "ingest",
            "--rule",
            rule.as_str(),
            "--dry-run",
        ],
    )
    .await;
    assert_ne!(code, Some(0), "{stdout} {stderr}");
    assert!(stdout.is_empty(), "{stdout}");

    // A rule that cannot run: the submission's refusal, in three parts.
    let (code, stdout, stderr) = run(
        nowhere(),
        &["ingest", "--rule", broken.as_str(), "--dry-run"],
    )
    .await;
    assert_eq!(code, Some(2), "{stdout} {stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("cannot read rule file") && stderr.contains("stray"),
        "the refusal names the file and the key at fault: {stderr}"
    );
    assert!(
        stderr.contains("hint:") && !stderr.contains("Location:"),
        "three parts, and no cause chain: {stderr}"
    );

    // A profile name outside the built-in set is refused, not replaced.
    let (code, stdout, stderr) = run(
        nowhere(),
        &[
            "--audit-profile",
            "no-such-profile",
            "ingest",
            "--rule",
            rule.as_str(),
            "--dry-run",
        ],
    )
    .await;
    assert_eq!(code, Some(2), "{stdout} {stderr}");
    assert!(stderr.contains("no-such-profile"), "{stderr}");

    // Without `--dry-run` the same rule is a submission, and there is
    // no daemon to take it.
    let (code, stdout, stderr) = run(
        bookrack_cmd!(&sandbox).library(LIBRARY),
        &["ingest", "--rule", rule.as_str()],
    )
    .await;
    assert_eq!(code, Some(2), "{stdout} {stderr}");
    assert!(stderr.contains("bookrack run"), "{stderr}");
}

#[tokio::test]
async fn a_rule_submission_reaches_the_queue_under_absolute_paths() {
    let sandbox = Sandbox::new();
    sandbox.name_data_dir(LIBRARY);
    let lock_path = sandbox.tty_lock_path();
    // The client runs in the sandbox's working directory.
    let material = sandbox.cwd().join("material");
    lay_out(&material);

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .library(LIBRARY)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(30))
        .await
        .expect("bookrack run did not become ready");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (code, _, stderr) = run(
            bookrack_cmd!(&sandbox).library(LIBRARY),
            &["queue", "pause"],
        )
        .await;
        if code == Some(0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never answered queue.pause: {stderr}",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A rule the daemon refuses writes no row and exits as bad input.
    let (code, stdout, stderr) = run(
        bookrack_cmd!(&sandbox).library(LIBRARY),
        &["ingest", "--rule", "material/broken.toml", "--no-wait"],
    )
    .await;
    assert_eq!(code, Some(2), "{stdout} {stderr}");
    assert!(
        stderr.contains("cannot read rule file") && stderr.contains("stray"),
        "{stderr}"
    );

    let (code, stdout, stderr) = run(
        bookrack_cmd!(&sandbox).library(LIBRARY),
        &["ingest", "--rule", "material/rule.toml", "--no-wait"],
    )
    .await;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stdout.starts_with("Queued rule.toml as ") && stdout.lines().count() == 1,
        "{stdout:?}"
    );

    let (code, stdout, stderr) = run(
        bookrack_cmd!(&sandbox).library(LIBRARY),
        &["--json", "queue", "list"],
    )
    .await;
    assert_eq!(code, Some(0), "{stderr}");
    let listing: Value = serde_json::from_str(&stdout).expect("queue list is JSON");
    let jobs = listing["jobs"].as_array().expect("jobs");
    assert_eq!(jobs.len(), 1, "the refused rule wrote no row: {listing}");
    let op = &jobs[0]["op"];
    assert_eq!(op["input"]["type"], "rule", "{listing}");
    let path = op["path"].as_str().expect("the rule's path");
    assert!(
        Path::new(path).is_absolute() && path.ends_with("material/rule.toml"),
        "the daemon was handed an absolute path: {path}"
    );

    daemon
        .stop(Duration::from_secs(10))
        .await
        .expect("stop the daemon");
}
