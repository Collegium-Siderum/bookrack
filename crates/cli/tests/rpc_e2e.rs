// SPDX-License-Identifier: Apache-2.0

//! End-to-end round-trip for `bookrack rpc call library.info` against a
//! live `bookrack run` daemon.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

mod common;

use std::time::Duration;

use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

#[tokio::test]
async fn library_info_round_trips_through_running_daemon() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();

    let mut daemon_cmd =
        Command::from(bookrack_cmd!(&sandbox).ollama_url(EmbedStub::url()).build());
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");

    assert!(
        wait_for_lock(&lock_path, Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    let output = Command::from(bookrack_cmd!(&sandbox).build())
        .arg("rpc")
        .arg("call")
        .arg("library.info")
        .arg("{}")
        .output()
        .await
        .expect("run bookrack rpc call");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "bookrack rpc call failed: status={:?}, stderr={}",
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
