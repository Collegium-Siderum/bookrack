// SPDX-License-Identifier: Apache-2.0

//! `bookrack queue wait` attaches to queued work through the command
//! entry and reports how each job ended.
//!
//! The queue document is seeded before the daemon starts, so the rows
//! carry controlled ids and cover the ended states the verb reports
//! without producing them through real ingests. The document is paused,
//! so the pending rows sit still until `queue resume`; the rows name a
//! library the daemon does not serve, so each run ends in a book-level
//! failure the test can wait for. The daemon serves a registered
//! library, so `--library` can be passed with a name that resolves.
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

const LIBRARY: &str = "main";

const ID_FAILED: &str = "0a000000-0000-7000-8000-000000000001";
const ID_DONE: &str = "0b000000-0000-7000-8000-000000000002";
const ID_CANCELLED: &str = "0c000000-0000-7000-8000-000000000003";
const ID_PENDING_D: &str = "0d000000-0000-7000-8000-000000000004";
const ID_PENDING_E: &str = "0e000000-0000-7000-8000-000000000005";

const QUEUED_AT: &str = "2026-01-02T03:04:05Z";
const FINISHED_AT: &str = "2026-01-02T03:05:05Z";
const FAILURE_TEXT: &str =
    "ingest: registry-mediated ingest: reading the source file failed: no such file";

fn row(id: &str, state: &str, error: Option<&str>) -> Value {
    let ended = state != "pending";
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
        "started_at": if ended { Value::String(QUEUED_AT.into()) } else { Value::Null },
        "finished_at": if ended { Value::String(FINISHED_AT.into()) } else { Value::Null },
        "error": error,
        "merged_into": null,
    })
}

fn seeded_document() -> Value {
    json!({
        "schema_version": QUEUE_SCHEMA_VERSION,
        "paused": true,
        "jobs": [
            row(ID_FAILED, "failed", Some(FAILURE_TEXT)),
            row(ID_DONE, "done", None),
            row(ID_CANCELLED, "cancelled", None),
            row(ID_PENDING_D, "pending", None),
            row(ID_PENDING_E, "pending", None),
        ],
    })
}

/// Run one client command and return its exit code, stdout and stderr.
async fn client(sandbox: &Sandbox, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::from(bookrack_cmd!(sandbox).without_data_dir().build())
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

/// Run one client command under `--json` and parse its stdout,
/// asserting the exit code.
async fn client_json(sandbox: &Sandbox, args: &[&str], expected: i32) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let (code, stdout, stderr) = client(sandbox, &full).await;
    assert_eq!(code, Some(expected), "{args:?}: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{args:?} is not JSON ({e}): {stdout}"))
}

fn job<'a>(listing: &'a Value, id: &str) -> &'a Value {
    listing["jobs"]
        .as_array()
        .and_then(|jobs| jobs.iter().find(|j| j["id"] == id))
        .unwrap_or_else(|| panic!("row {id} missing from {listing}"))
}

fn ids_and_states(report: &Value) -> Vec<(String, String)> {
    report["jobs"]
        .as_array()
        .unwrap_or_else(|| panic!("no jobs array in {report}"))
        .iter()
        .map(|j| {
            (
                j["job_id"].as_str().unwrap_or_default().to_string(),
                j["state"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn wait_reports_ended_and_awaited_jobs_through_the_command_entry() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root(LIBRARY);
    sandbox.write_registry_entries(Some(LIBRARY), &[(LIBRARY, root.as_path())]);
    std::fs::write(
        sandbox.daemon_state_dir().join("queue.json"),
        serde_json::to_vec(&seeded_document()).expect("serialise the seed"),
    )
    .expect("seed the queue document");

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.args(["--library", LIBRARY, "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&sandbox.tty_lock_path(), Duration::from_secs(30)).await,
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
    assert_eq!(
        listing["jobs"].as_array().map(Vec::len),
        Some(5),
        "{listing}"
    );
    assert_eq!(listing["paused"], true, "{listing}");

    // A paused queue holding a selected pending job is refused up front.
    for args in [vec!["queue", "wait"], vec!["queue", "wait", "--job", "0d"]] {
        let (code, _, stderr) = client(&sandbox, &args).await;
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains("paused"), "{args:?}: {stderr}");
        assert!(
            stderr.contains("bookrack queue resume"),
            "{args:?}: {stderr}"
        );
    }

    // `--library` has no effect on a queue verb: the worker probe must
    // not carry it into `status`, which refuses a selection.
    let (code, _, stderr) = client(
        &sandbox,
        &["--library", LIBRARY, "queue", "wait", "--job", "0d"],
    )
    .await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("paused"), "{stderr}");
    assert!(
        !stderr.contains("cannot run this command against library"),
        "the worker probe carried --library into status: {stderr}"
    );

    // A refused wait still reports what it saw: the ended job as a
    // record, the pending one by id. Under `--json` stdout is one
    // report and stderr one problem; under `--quiet` stdout is empty.
    let (code, stdout, stderr) = client(
        &sandbox,
        &["--json", "queue", "wait", "--job", "0a", "--job", "0d"],
    )
    .await;
    assert_eq!(code, Some(2), "{stderr}");
    let partial: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("a refused wait printed no report ({e}): {stdout}"));
    assert_eq!(
        ids_and_states(&partial),
        [(ID_FAILED.to_string(), "failed".to_string())]
    );
    assert_eq!(partial["unfinished"], json!([ID_PENDING_D]), "{partial}");
    let problem: Value = serde_json::from_str(&stderr)
        .unwrap_or_else(|e| panic!("a refused wait printed no problem ({e}): {stderr}"));
    assert!(
        problem["summary"]
            .as_str()
            .is_some_and(|s| s.contains("paused")),
        "{problem}"
    );
    let (code, stdout, _) = client(
        &sandbox,
        &["--quiet", "queue", "wait", "--job", "0a", "--job", "0d"],
    )
    .await;
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty(), "{stdout}");

    // `ingest` waits by default, and on a paused queue it is refused as
    // `queue wait` is, right after submitting: the job is queued, the
    // report names it under `unfinished`, and nothing waits a minute
    // for a stall.
    let source = sandbox.path().join("nowhere.epub");
    let started = Instant::now();
    let (code, stdout, stderr) =
        client(&sandbox, &["--json", "ingest", source.to_str().unwrap()]).await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the refusal took {:?}",
        started.elapsed()
    );
    let partial: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("a refused ingest wait printed no report ({e}): {stdout}"));
    assert_eq!(partial["jobs"], json!([]), "{partial}");
    let ingested_id = partial["unfinished"][0]
        .as_str()
        .unwrap_or_else(|| panic!("no unfinished id in {partial}"))
        .to_string();
    assert_eq!(partial["unfinished"].as_array().map(Vec::len), Some(1));
    assert!(stderr.contains("paused"), "{stderr}");
    assert!(stderr.contains("bookrack queue wait"), "{stderr}");
    let listing = client_json(&sandbox, &["queue", "list"], 0).await;
    assert_eq!(job(&listing, &ingested_id)["state"], "pending", "{listing}");

    // From here on the queue also holds the submitted job, whose id is a
    // UUIDv7 with a random tail; a fragment is matched anywhere in an id,
    // so a seeded row is named by eight characters of its head, which a
    // random tail does not reproduce.

    // One named job that already ended.
    let (code, stdout, stderr) = client(&sandbox, &["queue", "wait", "--job", "0b000000"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    let first = stdout.lines().next().unwrap_or_default();
    assert!(
        first.starts_with(&format!("Completed {ID_DONE}.epub as 00000002 in ")),
        "{stdout}"
    );
    assert!(first.ends_with("(done)"), "{stdout}");

    // Two named jobs that already ended, one of them failed.
    let report = client_json(
        &sandbox,
        &["queue", "wait", "--job", "0a000000", "--job", "0b000000"],
        5,
    )
    .await;
    assert_eq!(
        ids_and_states(&report),
        [
            (ID_FAILED.to_string(), "failed".to_string()),
            (ID_DONE.to_string(), "done".to_string()),
        ]
    );
    assert_eq!(report["totals"]["failed"], 1, "{report}");
    assert_eq!(report["jobs"][0]["error"], FAILURE_TEXT, "{report}");

    let (code, stdout, stderr) = client(
        &sandbox,
        &["queue", "wait", "--job", "0a000000", "--job", "0b000000"],
    )
    .await;
    assert_eq!(code, Some(5), "{stderr}");
    assert!(stdout.starts_with("Completed 2 jobs in "), "{stdout}");
    assert!(stdout.contains("(1 done, 1 failed)"), "{stdout}");
    assert!(
        stderr.contains(&format!("1/2 00000001 failed: {FAILURE_TEXT}")),
        "{stderr}"
    );

    // A named cancelled job fails the wait.
    let (code, _, stderr) = client(&sandbox, &["queue", "wait", "--job", "0c000000"]).await;
    assert_eq!(code, Some(5), "{stderr}");

    // The head and the tail of one id name one job.
    let report = client_json(
        &sandbox,
        &["queue", "wait", "--job", "0a000000", "--job", "00000001"],
        5,
    )
    .await;
    assert_eq!(report["jobs"].as_array().map(Vec::len), Some(1), "{report}");

    // Fragments that name no job, or more than one, are refused.
    let (code, _, stderr) = client(&sandbox, &["queue", "wait", "--job", "zz"]).await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("no job matches"), "{stderr}");
    let (code, _, stderr) = client(&sandbox, &["queue", "wait", "--job", "0"]).await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("ambiguous"), "{stderr}");
    assert!(stderr.contains("queue list --long"), "{stderr}");

    // An empty fragment is the parser's refusal, not the daemon's.
    let (code, _, stderr) = client(&sandbox, &["queue", "wait", "--job", ""]).await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(!stderr.contains("bookrack: "), "{stderr}");

    // Resumed, the two pending rows run and fail on their unknown
    // library. They may still be pending, running, or already failed
    // when the wait attaches; every branch reports the same outcome.
    let (code, _, stderr) = client(&sandbox, &["queue", "resume"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    let report = client_json(
        &sandbox,
        &[
            "queue",
            "wait",
            "--job",
            ID_PENDING_D,
            "--job",
            ID_PENDING_E,
        ],
        5,
    )
    .await;
    let mut outcomes = ids_and_states(&report);
    outcomes.sort();
    assert_eq!(
        outcomes,
        [
            (ID_PENDING_D.to_string(), "failed".to_string()),
            (ID_PENDING_E.to_string(), "failed".to_string()),
        ]
    );
    for j in report["jobs"].as_array().unwrap() {
        assert!(
            j["error"].as_str().is_some_and(|e| !e.is_empty()),
            "a failed job carries its error: {j}"
        );
    }

    // A retried job that finished before the wait is reported with the
    // outcome of the run just made, not the seeded one.
    let (code, _, stderr) = client(&sandbox, &["queue", "retry", "--job", "0a000000"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let rerun_finished_at = loop {
        let listing = client_json(&sandbox, &["queue", "list"], 0).await;
        let row = job(&listing, ID_FAILED);
        if row["state"] == "failed" && row["finished_at"] != FINISHED_AT {
            break row["finished_at"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "the retried row did not fail again: {row}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let report = client_json(&sandbox, &["queue", "wait", "--job", "0a000000"], 5).await;
    assert_eq!(report["jobs"].as_array().map(Vec::len), Some(1), "{report}");
    assert_eq!(report["jobs"][0]["state"], "failed", "{report}");
    assert_eq!(
        report["jobs"][0]["finished_at"], rerun_finished_at,
        "{report}"
    );

    // The ingested job runs once the queue is resumed and fails on its
    // missing source; whether it ends before or after the retried job
    // is not fixed, so it is waited on by id before the queue is read
    // as empty.
    let (code, _, stderr) = client(&sandbox, &["queue", "wait", "--job", &ingested_id]).await;
    assert_eq!(code, Some(5), "{stderr}");

    // Nothing pending or running is nothing to wait for, in each mode.
    let (code, stdout, stderr) = client(&sandbox, &["queue", "wait"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout.trim_end(), "nothing to wait for", "{stdout}");
    let report = client_json(&sandbox, &["queue", "wait"], 0).await;
    assert_eq!(report["jobs"], json!([]), "{report}");
    let (code, stdout, stderr) = client(&sandbox, &["--quiet", "queue", "wait"]).await;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(10)).await;
}
