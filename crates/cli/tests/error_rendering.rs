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

use tokio::process::Command;

use crate::common::{DaemonProcess, bookrack_bin, embed_stub_url, wait_for_lock};

/// Bring a daemon up on a scratch data root and run `args` through it,
/// returning the client invocation's exit status and stderr.
async fn client_stderr_against_daemon(args: &[&str]) -> (Option<i32>, String) {
    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let lock_path = runtime_dir.path().join("bookrack.tty.lock");

    let daemon = DaemonProcess::spawn(
        Command::new(bookrack_bin())
            .arg("run")
            .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
            .env("BOOKRACK_DATA_DIR", data_dir.path())
            .env("BOOKRACK_OLLAMA_URL", embed_stub_url()),
    )
    .expect("spawn bookrack run");

    assert!(
        wait_for_lock(&lock_path, Duration::from_secs(30)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    let output = Command::new(bookrack_bin())
        .args(args)
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
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

/// A search naming paper ids on the book side. The point is not the
/// rejection — that already worked — but that the daemon's `data`
/// object survives the socket, the control client, and
/// `classify_eyre`, and is still there when the reporter runs.
const REJECTED_SEARCH: &str = r#"{"query":"x","kind":"book","exclude_paper_intake_ids":[1]}"#;

#[tokio::test]
async fn a_rejected_call_reports_one_line_and_exits_two() {
    let (code, stderr) =
        client_stderr_against_daemon(&["exec", "library.search", REJECTED_SEARCH]).await;

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
        client_stderr_against_daemon(&["--json", "exec", "library.search", REJECTED_SEARCH]).await;

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
    let scratch = tempfile::tempdir().expect("tempdir");
    let not_a_dir = scratch.path().join("not-a-directory");
    std::fs::write(&not_a_dir, b"").expect("write file");
    let registry = scratch.path().join("registry.toml");
    std::fs::write(&registry, b"").expect("write registry");

    for extra in [&[][..], &["--json"][..]] {
        let mut args: Vec<&str> = extra.to_vec();
        args.extend_from_slice(&["--data-dir", not_a_dir.to_str().unwrap(), "doctor"]);
        let output = Command::new(bookrack_bin())
            .args(&args)
            .env("BOOKRACK_RUNTIME_DIR", scratch.path().join("runtime"))
            .env("BOOKRACK_REGISTRY", &registry)
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
    let scratch = tempfile::tempdir().expect("tempdir");
    let not_a_dir = scratch.path().join("not-a-directory");
    std::fs::write(&not_a_dir, b"").expect("write file");
    let registry = scratch.path().join("registry.toml");
    std::fs::write(&registry, b"").expect("write registry");

    let output = Command::new(bookrack_bin())
        .args(["--data-dir", not_a_dir.to_str().unwrap(), "run"])
        .env("BOOKRACK_RUNTIME_DIR", scratch.path().join("runtime"))
        .env("BOOKRACK_REGISTRY", &registry)
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

/// A classified failure with no `data` prints exactly what it printed
/// before the split — one line, no empty continuation.
#[tokio::test]
async fn a_failure_without_a_diagnostic_stays_a_single_line() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let registry = scratch.path().join("registry.toml");
    std::fs::write(&registry, b"").expect("write registry");

    let output = Command::new(bookrack_bin())
        .args(["exec", "library.info", "{}"])
        .env("BOOKRACK_RUNTIME_DIR", scratch.path().join("runtime"))
        .env("BOOKRACK_REGISTRY", &registry)
        .output()
        .await
        .expect("run bookrack exec");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        stderr.trim().lines().count(),
        1,
        "expected one line, got: {stderr}"
    );
    assert!(stderr.starts_with("bookrack: "), "{stderr}");
}
