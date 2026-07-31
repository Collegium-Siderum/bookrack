// SPDX-License-Identifier: Apache-2.0

//! `bookrack run` against an embed backend that cannot serve.
//!
//! This is the failure the whole error-message series started from.
//! It used to surface as a four-line `color-eyre` chain ending in an
//! escaped HTTP body, after the daemon had already paid for a
//! reranker start and begun warming libraries. The contract now:
//! refuse before any of that, in one sentence with one command, at
//! exit 2.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;

use tokio::process::Command;

use crate::common::bookrack_bin;

/// A loopback stand-in for Ollama that is up and answering but holds
/// no models — the state an operator lands in after a fresh install,
/// and the one that used to fail deep inside library warm-up.
fn spawn_empty_ollama() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let url = format!("http://{}", listener.local_addr().expect("stub addr"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut socket) = stream else { continue };
            let mut scratch = [0u8; 8192];
            let _ = socket.read(&mut scratch);
            let body = r#"{"models":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.flush();
        }
    });
    url
}

async fn run_against_empty_ollama(extra: &[&str]) -> (Option<i32>, String) {
    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let registry = runtime_dir.path().join("registry.toml");
    std::fs::write(&registry, b"").expect("write registry");

    let mut args: Vec<&str> = extra.to_vec();
    args.push("run");
    let output = Command::new(bookrack_bin())
        .args(&args)
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env("BOOKRACK_DATA_DIR", data_dir.path())
        .env("BOOKRACK_REGISTRY", &registry)
        .env("BOOKRACK_OLLAMA_URL", spawn_empty_ollama())
        .output()
        .await
        .expect("run bookrack run");

    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn an_unusable_embed_backend_exits_two_before_bring_up() {
    let (code, stderr) = run_against_empty_ollama(&[]).await;

    assert_eq!(
        code,
        Some(2),
        "an unusable backend is operator input: {stderr}"
    );
    assert!(
        stderr.contains("ollama pull"),
        "the hint must name the repair: {stderr}"
    );
    assert!(
        stderr.contains("is not available on the Ollama daemon"),
        "the summary must say what failed, in words: {stderr}"
    );

    // The refusal happens before step 6 opens a library, and before
    // 5b may spawn and wait on a llama-server. Both markers would
    // appear in the failure text if the sequence had got that far.
    assert!(
        !stderr.contains("bring up library"),
        "the refusal must precede library warm-up: {stderr}"
    );
    assert!(
        !stderr.contains("bring up the reranker"),
        "the refusal must precede the reranker start: {stderr}"
    );

    assert!(
        !stderr.contains("embed error") && !stderr.contains("\\\""),
        "no module name and no escaped payload reach the operator: {stderr}"
    );
}

/// The refusal goes through the same typed path as every other
/// classified failure, so `--json` gets the structured object rather
/// than the prose that a flattened `LocalUserError` would have given.
#[tokio::test]
async fn the_refusal_is_structured_under_json() {
    let (code, stderr) = run_against_empty_ollama(&["--json"]).await;

    assert_eq!(code, Some(2), "{stderr}");
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {stderr}"));
    assert!(
        parsed
            .get("hint")
            .and_then(|v| v.as_str())
            .is_some_and(|h| h.contains("ollama pull")),
        "{parsed}"
    );
    assert_eq!(
        parsed.get("retryable"),
        Some(&serde_json::Value::Bool(false)),
        "a model that is not pulled will not appear on a retry: {parsed}"
    );
}
