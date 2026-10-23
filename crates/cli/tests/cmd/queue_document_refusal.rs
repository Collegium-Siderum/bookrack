// SPDX-License-Identifier: Apache-2.0

//! `bookrack run` against a queue document a later binary wrote.
//!
//! Downgrading is what produces one: the document spans libraries and
//! lives in the daemon state directory, so it outlives any data root
//! the operator swaps in. The daemon refuses it rather than loading
//! it, because a load would drop the keys this binary has no field for
//! and the next write would persist the truncated document.
//!
//! A refusal is operator input, not a bug: exit 2, the three parts,
//! and no `color-eyre` chain. The document itself must still be on
//! disk afterwards — the version that wrote it reads every job in it.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

use bookrack_core::queue::QUEUE_SCHEMA_VERSION;
use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

/// Seed a queue document one schema version above this binary's and
/// run the daemon against it. Returns the exit code, stdout, stderr,
/// and the document as it stands after the run.
async fn run_against_a_newer_queue_document(
    extra: &[&str],
) -> (Option<i32>, String, String, String, String) {
    let sandbox = Sandbox::new();
    let queue_path = sandbox.daemon_state_dir().join("queue.json");
    let doc = format!(
        "{{\"schema_version\": {}, \"paused\": false, \"jobs\": [], \
         \"future_field\": \"keep me\"}}",
        QUEUE_SCHEMA_VERSION + 1
    );
    std::fs::write(&queue_path, doc.as_bytes()).expect("seed the queue document");

    let mut args: Vec<&str> = extra.to_vec();
    args.push("run");
    let output = Command::from(bookrack_cmd!(&sandbox).ollama_url(EmbedStub::url()).build())
        .args(&args)
        .output()
        .await
        .expect("run bookrack run");

    let after = std::fs::read_to_string(&queue_path).expect("read the queue document back");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        doc,
        after,
    )
}

#[tokio::test]
async fn a_newer_queue_document_exits_two_and_survives_the_refusal() {
    let (code, stdout, stderr, before, after) = run_against_a_newer_queue_document(&[]).await;

    assert_eq!(
        code,
        Some(2),
        "a document from a newer version is operator input: {stderr}"
    );
    assert!(
        stderr.contains("newer version of bookrack"),
        "the summary must say what failed, in words: {stderr}"
    );
    assert!(
        stderr.contains(&(QUEUE_SCHEMA_VERSION + 1).to_string())
            && stderr.contains(&QUEUE_SCHEMA_VERSION.to_string()),
        "the detail must name both versions: {stderr}"
    );
    assert!(
        !stdout.contains("bookrack daemon running"),
        "a refused bring-up must announce nothing: {stdout}"
    );
    assert_eq!(
        after, before,
        "the refused document must be left exactly as it was found"
    );
}

/// The refusal takes the same typed path as every other classified
/// failure, so `--json` gets the object rather than prose.
#[tokio::test]
async fn the_refusal_is_structured_under_json() {
    let (code, _stdout, stderr, _before, _after) =
        run_against_a_newer_queue_document(&["--json"]).await;

    assert_eq!(code, Some(2), "{stderr}");
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {stderr}"));
    assert!(
        parsed
            .get("hint")
            .and_then(|v| v.as_str())
            .is_some_and(|h| h.contains("move the file aside")),
        "the hint must name a way out that does not destroy the document: {parsed}"
    );
    assert_eq!(
        parsed.get("retryable"),
        Some(&serde_json::Value::Bool(false)),
        "{parsed}"
    );
}
