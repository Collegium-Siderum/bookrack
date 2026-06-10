// SPDX-License-Identifier: Apache-2.0

//! End-to-end check that a control-plane shutdown actually terminates
//! the `bookrack run` process. `bookrack quit` drains the daemon over
//! the control socket; the process must then exit on its own — a
//! foreground task that blocks an OS thread would stall the tokio
//! runtime's teardown and leave a drained-but-alive process behind.
//!
//! Marked `#[ignore]` because the daemon's startup path opens a
//! `bookrack_query::Library`, which probes the configured embedder
//! (Ollama by default). Run it manually:
//!
//! ```text
//! cargo test -p bookrack-cli --test quit_e2e -- --ignored --nocapture
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
async fn quit_terminates_the_daemon_process() {
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
        .arg("quit")
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .output()
        .await
        .expect("run bookrack quit");
    assert!(
        output.status.success(),
        "bookrack quit failed: status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let exit = tokio::time::timeout(Duration::from_secs(5), daemon.wait())
        .await
        .expect("daemon did not exit within 5s of `bookrack quit`")
        .expect("daemon wait failed");
    assert!(exit.success(), "daemon exited non-zero: {exit:?}");

    let mut stdout = String::new();
    if let Some(mut pipe) = daemon.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout).await;
    }
    assert!(
        stdout.contains("bookrack daemon running:"),
        "expected startup banner on stdout, got: {stdout}",
    );
}
