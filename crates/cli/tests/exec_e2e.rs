// SPDX-License-Identifier: Apache-2.0

//! End-to-end round-trip for `bookrack exec library.info` against a
//! live `bookrack run` daemon.
//!
//! Marked `#[ignore]` because the daemon's startup path opens a
//! `bookrack_query::Library`, which probes the configured embedder
//! (Ollama by default). That is a real network call that cannot
//! be made on a CI runner without a local Ollama or a stand-in.
//!
//! Run it manually after wiring up Ollama against the configured
//! `BOOKRACK_DATA_DIR`:
//!
//! ```text
//! cargo test -p bookrack-cli --test exec_e2e -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Command;

fn bookrack_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bookrack"))
}

async fn wait_for_lock(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if text.contains("mcp=") {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
#[ignore]
async fn library_info_round_trips_through_running_daemon() {
    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let lock_path = runtime_dir.path().join("bookrack.tty.lock");

    let mut daemon = Command::new(bookrack_bin())
        .arg("run")
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env("BOOKRACK_DATA_DIR", data_dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn bookrack run");

    assert!(
        wait_for_lock(&lock_path, Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    let output = Command::new(bookrack_bin())
        .arg("exec")
        .arg("library.info")
        .arg("{}")
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .output()
        .await
        .expect("run bookrack exec");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "bookrack exec failed: status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("data_dir"),
        "expected `data_dir` in tool result, got: {stdout}",
    );

    if let Some(id) = daemon.id() {
        // Send SIGTERM via nix-style signal; on stable, kill is SIGKILL,
        // so we use a separate `kill -15` call to stay graceful.
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let exit = tokio::time::timeout(Duration::from_secs(5), daemon.wait()).await;
    assert!(
        exit.is_ok(),
        "daemon did not exit within 5 s of SIGTERM; live stdout would be lost"
    );

    // Drain any remaining stdout so the assertion failure messages
    // above stay useful while developing the test locally.
    if let Some(mut stdout) = daemon.stdout.take() {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf).await;
    }
}
