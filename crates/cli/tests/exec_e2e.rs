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

mod common;

use std::time::Duration;

use tokio::process::Command;

use crate::common::{DaemonProcess, bookrack_bin, wait_for_lock};

#[tokio::test]
#[ignore]
async fn library_info_round_trips_through_running_daemon() {
    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    let data_dir = tempfile::tempdir().expect("data tempdir");
    let lock_path = runtime_dir.path().join("bookrack.tty.lock");

    let daemon = DaemonProcess::spawn(
        Command::new(bookrack_bin())
            .arg("run")
            .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
            .env("BOOKRACK_DATA_DIR", data_dir.path()),
    )
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
        // On stable Rust `Child::kill` is SIGKILL, so a separate
        // `kill -15` is used to drive a graceful shutdown.
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let (status, _stdout, _stderr) = daemon
        .wait_with_output(Duration::from_secs(5))
        .await
        .expect("daemon must exit within 5 s of SIGTERM");
    assert!(
        status.code().is_some() || status.success(),
        "daemon exited abnormally: {status:?}",
    );
}
