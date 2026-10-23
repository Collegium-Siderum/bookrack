// SPDX-License-Identifier: Apache-2.0

//! `bookrack init` refused on its input or the host.
//!
//! The wizard runs before `Config::resolve`, so none of the typed
//! failures the resolver produces reach it; its own refusals used to
//! surface as the fallback reporter's cause chain at exit 1, where
//! every other predictable failure of the binary is one line with a
//! hint at exit 2. The contract pinned here: a refusal the operator can
//! act on is reported like the rest.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;

use bookrack_test_support::{Sandbox, bookrack_cmd};
use tokio::process::Command;

/// A loopback stand-in for Ollama that is up and answering but holds
/// no models.
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

async fn run_init(
    sandbox: &Sandbox,
    ollama_url: Option<String>,
    args: &[&str],
) -> (Option<i32>, String) {
    let mut spawn = bookrack_cmd!(sandbox).without_data_dir();
    if let Some(url) = ollama_url {
        spawn = spawn.ollama_url(url);
    }
    let output = Command::from(spawn.build())
        .arg("init")
        .args(args)
        .output()
        .await
        .expect("run bookrack init");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn a_non_interactive_init_without_a_data_dir_exits_two_with_a_hint() {
    let sandbox = Sandbox::new();
    let (code, stderr) = run_init(&sandbox, None, &["--non-interactive"]).await;

    assert_eq!(
        code,
        Some(2),
        "a missing --data-dir is operator input: {stderr}"
    );
    assert!(
        stderr.starts_with("bookrack: cannot choose a data root"),
        "the first line is the one-line summary: {stderr}"
    );
    assert!(
        stderr.lines().any(|l| l.trim_start().starts_with("hint:")),
        "the reporter draws the hint: {stderr}"
    );
    assert!(
        stderr.contains("--data-dir"),
        "the hint names the flag: {stderr}"
    );
}

#[tokio::test]
async fn an_unpulled_embed_model_is_reported_once_with_its_pull_command() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("fresh");
    let root = root.to_str().expect("utf-8 path");
    let (code, stderr) = run_init(
        &sandbox,
        Some(spawn_empty_ollama()),
        &["--non-interactive", "--no-smoke", "--data-dir", root],
    )
    .await;

    assert_eq!(
        code,
        Some(2),
        "an unpulled model is operator input: {stderr}"
    );
    assert!(
        stderr.contains("hint:"),
        "the reporter draws the hint: {stderr}"
    );
    assert_eq!(
        stderr.matches("ollama pull").count(),
        1,
        "the remedy is printed once, by the reporter: {stderr}"
    );
}

/// The refusal goes through the typed path, so `--json` gets the
/// structured object rather than prose.
#[tokio::test]
async fn the_refusal_is_structured_under_json() {
    let sandbox = Sandbox::new();
    let output = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--json", "init", "--non-interactive"])
        .output()
        .await
        .expect("run bookrack init");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(output.status.code(), Some(2), "{stderr}");
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {stderr}"));
    assert!(
        parsed
            .get("hint")
            .and_then(|v| v.as_str())
            .is_some_and(|h| h.contains("--data-dir")),
        "{parsed}"
    );
}
