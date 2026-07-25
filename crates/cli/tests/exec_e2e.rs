// SPDX-License-Identifier: Apache-2.0

//! End-to-end round-trip for `bookrack exec library.info` against a
//! live `bookrack run` daemon.
//!
//! The embedder probe on the daemon's startup path is answered by the
//! loopback stub in `common`, so no Ollama daemon is required.

mod common;

use std::time::Duration;

use tokio::process::Command;

use crate::common::{DaemonProcess, bookrack_bin, wait_for_lock};

#[tokio::test]
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
    let (status, _stdout, stderr) = daemon
        .wait_with_output(Duration::from_secs(5))
        .await
        .expect("daemon must exit within 5 s of SIGTERM");
    // A signalled shutdown unwinds the session and leaves through
    // `std::process::exit(0)`, so the only acceptable status is a clean
    // zero: dying from the signal itself, or unwinding into an error
    // code, both mean the graceful path did not run. The session lock
    // file is left on disk on purpose — it carries an advisory flock
    // the OS releases at exit — so its presence afterwards is not a
    // leak and is deliberately not asserted here.
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM must produce a clean exit: status={status:?} stderr={stderr}",
    );
}
