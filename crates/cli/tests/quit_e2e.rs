// SPDX-License-Identifier: Apache-2.0

//! End-to-end check that a control-plane shutdown actually terminates
//! the `bookrack run` process. `bookrack quit` drains the daemon over
//! the control socket; the process must then exit on its own — a
//! foreground task that blocks an OS thread would stall the tokio
//! runtime's teardown and leave a drained-but-alive process behind.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

mod common;

use std::time::Duration;

use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

#[tokio::test]
async fn quit_terminates_the_daemon_process() {
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
        .arg("quit")
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
