// SPDX-License-Identifier: Apache-2.0

//! How the binary presents a failure on stderr.
//!
//! Four surfaces, each with its own contract: a daemon-side rejection
//! arrives with its `data` object intact and renders through it; the
//! same failure under `--json` renders as one parseable object; a
//! variant whose own renderer already drew the failure is not drawn a
//! second time; and an error the CLI cannot classify keeps its full
//! `color-eyre` cause chain, because that is the only handle anyone
//! has on a genuine bug.

#![cfg(unix)]

mod common;

use std::time::Duration;

use bookrack_test_support::{EmbedFailure, EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

/// Bring a daemon up on a scratch data root and run `args` through it,
/// returning the client invocation's exit status and stderr.
async fn client_stderr_against_daemon(args: &[&str]) -> (Option<i32>, String) {
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

    let output = Command::from(bookrack_cmd!(&sandbox).build())
        .args(args)
        .output()
        .await
        .expect("run bookrack client");

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(10)).await;

    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A synthetic text long enough for the ingest pipeline to extract
/// prose, plan chunks, and embed them against the stub.
const BOOK_TEXT: &str = "\
Chapter One

The synthetic narrator opened the synthetic ledger and began to
count. Every entry in the ledger described a fictional object, and
every fictional object had a fictional weight. Counting the weights
took the narrator most of the morning.

By noon the ledger was balanced. The narrator celebrated with a
fictional meal and recorded the celebration as a footnote. Footnotes,
the narrator believed, were the highest form of bookkeeping.

Chapter Two

On the second day the ledger grew a second column. The new column
held colours instead of weights, and the narrator sorted them from
dull to bright. Sorting colours was slower than counting weights.

When the sorting ended the narrator closed the ledger, satisfied
that both columns agreed with each other in every fictional respect.
";

/// Ingest one synthetic book against a healthy stub, switch the stub to
/// `failure`, then re-embed that book — returning the re-embed's exit
/// status and stderr.
///
/// Two constraints pick this vehicle over the shorter
/// `stamps reconcile`. Bring-up opens each library by probing the
/// embedder, so the switch cannot be thrown before the daemon answers
/// or the daemon never starts; and a successful probe caches the
/// observed dimension per `(model, base_url)` for the process, so every
/// later single-batch probe of the same text is answered without HTTP.
/// A re-embed sends real chunk text, which the cache never covers.
async fn reembed_against_a_failing_backend(failure: EmbedFailure) -> (Option<i32>, String) {
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

    let book_path = sandbox.path().join("synthetic-ledger.txt");
    std::fs::write(&book_path, BOOK_TEXT).expect("write the synthetic book");

    // The session lock lands before the libraries are warm, so the
    // ingest doubles as the wait: it is answered only once the daemon
    // serves, and it leaves the chunk rows the re-embed below rewrites.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let ingest = Command::from(bookrack_cmd!(&sandbox).build())
            .args(["ingest", book_path.to_str().expect("utf-8 path")])
            .output()
            .await
            .expect("run bookrack ingest");
        if ingest.status.success() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fixture ingest never succeeded: {}",
            String::from_utf8_lossy(&ingest.stderr),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    EmbedStub::set_failure(failure);
    let output = Command::from(bookrack_cmd!(&sandbox).build())
        .args(["vectors", "reembed", "--yes"])
        .output()
        .await
        .expect("run bookrack vectors reembed");

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(10)).await;

    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A search naming paper ids on the book side. The point is not the
/// rejection — that already worked — but that the daemon's `data`
/// object survives the socket, the control client, and
/// `classify_eyre`, and is still there when the reporter runs.
const REJECTED_SEARCH: &str = r#"{"query":"x","kind":"book","exclude_paper_intake_ids":[1]}"#;

#[tokio::test]
async fn a_rejected_call_reports_one_line_and_exits_two() {
    let (code, stderr) =
        client_stderr_against_daemon(&["rpc", "call", "library.search", REJECTED_SEARCH]).await;

    assert_eq!(code, Some(2), "a caller-input rejection exits 2: {stderr}");
    assert!(stderr.starts_with("bookrack: "), "{stderr}");
    assert!(
        !stderr.contains("query error") && !stderr.contains("ops error"),
        "a module name is not a failure: {stderr}"
    );
    assert!(
        !stderr.contains("\\\""),
        "no escaped JSON reaches the operator: {stderr}"
    );
}

/// `--json` had no structured failure path at all: success gave JSON,
/// failure gave prose, so a scripted caller had to parse two shapes.
/// `retryable` is the field that makes the object worth parsing — it
/// is what an agent branches on instead of reading the wording.
#[tokio::test]
async fn a_json_run_reports_the_failure_as_one_parseable_object() {
    let (code, stderr) =
        client_stderr_against_daemon(&["--json", "rpc", "call", "library.search", REJECTED_SEARCH])
            .await;

    let parsed: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {stderr}"));
    assert!(
        parsed.get("summary").and_then(|v| v.as_str()).is_some(),
        "{parsed}"
    );
    assert_eq!(
        parsed.get("retryable"),
        Some(&serde_json::Value::Bool(false)),
        "the daemon's data object must reach the JSON report: {parsed}"
    );
    assert_eq!(code, Some(2));
}

/// `doctor` draws its own table, so the reporter must stay out of the
/// way. Adding a summary line — or, under `--json`, a whole second
/// error object — would report the same failure twice.
#[tokio::test]
async fn a_self_reported_error_is_not_rendered_twice() {
    let sandbox = Sandbox::new();
    let not_a_dir = sandbox.path().join("not-a-directory");
    std::fs::write(&not_a_dir, b"").expect("write file");

    for extra in [&[][..], &["--json"][..]] {
        let mut args: Vec<&str> = extra.to_vec();
        args.extend_from_slice(&["--data-dir", not_a_dir.to_str().unwrap(), "doctor"]);
        let output = Command::from(bookrack_cmd!(&sandbox).build())
            .args(&args)
            .output()
            .await
            .expect("run bookrack doctor");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "doctor must exit 1: {stderr}"
        );
        assert!(
            stderr.trim().is_empty(),
            "doctor already drew the failure; the reporter must add nothing ({args:?}): {stderr}"
        );
    }
}

/// The three-part rendering only applies to failures the CLI
/// classified. An unclassified one is, by construction, a suspected
/// bug: collapsing it to a one-line summary would take away the cause
/// chain that makes it reportable.
#[tokio::test]
async fn an_unclassified_error_still_prints_the_full_cause_chain() {
    let sandbox = Sandbox::new();
    let not_a_dir = sandbox.path().join("not-a-directory");
    std::fs::write(&not_a_dir, b"").expect("write file");

    let output = Command::from(bookrack_cmd!(&sandbox).build())
        .args(["--data-dir", not_a_dir.to_str().unwrap(), "run"])
        .output()
        .await
        .expect("run bookrack run");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("is not an existing directory"),
        "the cause must survive: {stderr}"
    );
    assert!(
        stderr.contains("0:"),
        "the numbered color-eyre cause chain must survive: {stderr}"
    );
    assert!(
        !stderr.contains("bookrack: "),
        "an unclassified error is not a one-line report: {stderr}"
    );
}

/// The whole chain for an absent embedding model, end to end: the
/// daemon classifies it as caller input, the CLI registers the code,
/// and the hint the embed error wrote for itself reaches stderr.
///
/// The stderr assertion is not decoration: with the exit code alone,
/// a variant left out of `problem_data()` would still pass while the
/// operator lost the one sentence that says what to do.
#[tokio::test]
async fn an_absent_embedding_model_exits_two_and_names_the_pull_command() {
    let (code, stderr) = reembed_against_a_failing_backend(EmbedFailure::ModelNotFound).await;

    assert_eq!(
        code,
        Some(2),
        "an unpulled model is operator input: {stderr}"
    );
    assert!(
        stderr.contains("ollama pull"),
        "the repair must reach the operator: {stderr}"
    );
}

/// The same command against an overloaded backend: nothing about the
/// request is wrong, and the same call may succeed once the load
/// clears, so it takes the retryable bucket instead — a distinct code,
/// a distinct exit, and still its own hint.
#[tokio::test]
async fn an_overloaded_backend_exits_four_and_keeps_its_hint() {
    let (code, stderr) = reembed_against_a_failing_backend(EmbedFailure::Overloaded).await;

    assert_eq!(
        code,
        Some(4),
        "an overloaded backend is retryable: {stderr}"
    );
    assert!(
        stderr.contains("smaller batch"),
        "the hint must survive the CLI's own classification: {stderr}"
    );
}

/// A classified failure with no `data` prints exactly what it printed
/// before the split — one line, no empty continuation.
#[tokio::test]
async fn a_failure_without_a_diagnostic_stays_a_single_line() {
    let sandbox = Sandbox::new();

    let output = Command::from(bookrack_cmd!(&sandbox).build())
        .args(["rpc", "call", "library.info", "{}"])
        .output()
        .await
        .expect("run bookrack rpc call");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        stderr.trim().lines().count(),
        1,
        "expected one line, got: {stderr}"
    );
    assert!(stderr.starts_with("bookrack: "), "{stderr}");
}
