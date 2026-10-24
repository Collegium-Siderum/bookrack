// SPDX-License-Identifier: Apache-2.0

//! Pins the promises of the process plumbing in `common`, using `sh`
//! as the child so no daemon is involved.

#![cfg(unix)]

use std::time::Duration;

use tokio::process::Command;

use crate::common::DaemonProcess;

fn sh(script: &str) -> DaemonProcess {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    DaemonProcess::spawn(cmd).expect("spawn sh")
}

/// A grandchild that inherits stdout keeps the pipe open after the
/// child exits; the run must report that, carrying what was read.
#[tokio::test]
async fn an_unfinished_capture_is_a_failure() {
    let child = sh("sleep 30 & echo started");
    let failure = match child.exit_within(Duration::from_secs(5)).await {
        Err(failure) => failure,
        Ok(exit) => panic!(
            "an unfinished stdout drain must be a failure, got Ok: \
             status={:?} stdout={:?} stderr={:?}",
            exit.status, exit.stdout, exit.stderr,
        ),
    };
    assert!(
        failure.why.contains("stdout was still open"),
        "the failure must name the stream: {}",
        failure.why,
    );
    assert_eq!(
        failure.stdout, "started\n",
        "the failure must carry what was read"
    );
    assert_eq!(
        failure.stderr, "",
        "stderr reached EOF and must come back empty"
    );
}

/// A child that outlives the deadline is killed and reported, with
/// what it had printed by then.
#[tokio::test]
async fn a_child_past_its_deadline_is_killed_and_reported() {
    let child = sh("echo begun; sleep 30");
    let failure = match child.exit_within(Duration::from_secs(1)).await {
        Err(failure) => failure,
        Ok(exit) => panic!(
            "a child past its deadline must be a failure, got Ok: {:?}",
            exit.status
        ),
    };
    assert!(
        failure.why.starts_with("the child did not exit within 1s"),
        "the failure must name the deadline: {}",
        failure.why,
    );
    assert_eq!(
        failure.stdout, "begun\n",
        "the failure must carry what was read"
    );
}

/// Output well past the kernel pipe buffer on both streams comes back
/// whole, and the run is `Ok`.
#[tokio::test]
async fn a_large_output_is_captured_whole() {
    const LINES: usize = 20_000;
    let child = sh(&format!(
        "i=0; while [ $i -lt {LINES} ]; do echo 0123456789; echo abcdefghij >&2; i=$((i+1)); done"
    ));
    let exit = child
        .exit_within(Duration::from_secs(30))
        .await
        .expect("a child that exits with both streams closed is Ok");
    assert!(exit.status.success(), "{:?}", exit.status);
    assert_eq!(exit.stdout.len(), LINES * 11, "stdout is whole");
    assert_eq!(exit.stderr.len(), LINES * 11, "stderr is whole");
}
