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

mod common;

use std::time::Duration;

use tokio::process::Command;

use crate::common::{DaemonProcess, bookrack_bin, wait_for_lock};

#[tokio::test]
#[ignore]
async fn quit_terminates_the_daemon_process() {
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

    let (status, stdout, _stderr) = daemon
        .wait_with_output(Duration::from_secs(5))
        .await
        .expect("daemon must exit within 5 s of `bookrack quit`");
    assert!(status.success(), "daemon exited non-zero: {status:?}");
    assert!(
        stdout.contains("bookrack daemon running:"),
        "expected startup banner on stdout, got: {stdout}",
    );
}
