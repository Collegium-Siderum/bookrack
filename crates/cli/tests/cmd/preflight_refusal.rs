// SPDX-License-Identifier: Apache-2.0

//! `bookrack run` against a dependency it cannot use.
//!
//! The embed backend is the failure the whole error-message series
//! started from. It used to surface as a four-line `color-eyre` chain
//! ending in an escaped HTTP body, after the daemon had already paid
//! for a reranker start and begun warming libraries. The contract
//! now: refuse before any of that, in one sentence with one command,
//! at exit 2.
//!
//! The MCP endpoint answers to the same contract, and adds one clause
//! the backend case cannot express: a refusal must not be preceded by
//! a success line. An endpoint that reports itself served, then fails
//! inside its own task, leaves every health surface quoting an
//! address another process answers on.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;

use bookrack_test_support::{Sandbox, bookrack_cmd};
use tokio::process::Command;

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
    let sandbox = Sandbox::new();

    let mut args: Vec<&str> = extra.to_vec();
    args.push("run");
    let output = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(spawn_empty_ollama())
            .build(),
    )
    .args(&args)
    .output()
    .await
    .expect("run bookrack run");

    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `bookrack run` pointed at an address this test holds. The Ollama
/// stand-in is the empty one, so a refusal that quoted a pull command
/// would mean the endpoint was not checked first.
async fn run_against_a_held_mcp_address(extra: &[&str]) -> (Option<i32>, String, String) {
    let sandbox = Sandbox::new();
    let occupant = TcpListener::bind("127.0.0.1:0").expect("hold an address");
    let addr = occupant.local_addr().expect("addr").to_string();

    let mut args: Vec<&str> = extra.to_vec();
    args.extend_from_slice(&["run", "--mcp-addr", &addr]);
    let output = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(spawn_empty_ollama())
            .build(),
    )
    .args(&args)
    .output()
    .await
    .expect("run bookrack run");

    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
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

#[tokio::test]
async fn an_occupied_mcp_address_exits_two_and_prints_no_success_line() {
    let (code, stdout, stderr) = run_against_a_held_mcp_address(&[]).await;

    assert_eq!(
        code,
        Some(2),
        "an address held by someone else is operator input: {stderr}"
    );
    assert!(
        stderr.contains("cannot serve MCP on"),
        "the summary must say what failed, in words: {stderr}"
    );
    assert!(
        stderr.contains("BOOKRACK_MCP_ADDR") || stderr.contains("--mcp-addr"),
        "the hint must name a way out: {stderr}"
    );

    // The clause the embed case cannot express: nothing may announce
    // the daemon before the endpoint is in hand. A success line here
    // is what sent operators to a `curl` that answered as someone
    // else's service.
    assert!(
        !stdout.contains("bookrack daemon running"),
        "a refused bring-up must announce nothing: {stdout}"
    );

    // The endpoint is taken before the backend is probed, so the
    // cheaper check is what the operator hears about first.
    assert!(
        !stderr.contains("ollama pull"),
        "the endpoint must be checked before the embed backend: {stderr}"
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
