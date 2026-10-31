// SPDX-License-Identifier: Apache-2.0

//! Pins the promises of `bookrack_test_support::DaemonProcess`. `sh`
//! stands in for the child wherever the promise is about the plumbing
//! itself; a real `bookrack run` is the child where it is about how the
//! daemon's readiness looks from outside.

#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

fn sh(script: &str) -> DaemonProcess {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    DaemonProcess::spawn(cmd).expect("spawn sh")
}

/// [`sh`] with positional parameters, so paths reach the script as
/// `$1`, `$2` instead of being quoted into it.
fn sh_with(script: &str, args: &[&OsStr]) -> DaemonProcess {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).arg("sh").args(args);
    DaemonProcess::spawn(cmd).expect("spawn sh")
}

/// Writes a session lock naming the shell itself, the way a daemon
/// that has taken the lock and bound its sockets leaves it.
const WRITE_OWN_LOCK: &str =
    "printf 'pid=%s\\nmcp=127.0.0.1:1\\ncontrol_sock=%s\\n' $$ \"$2\" > \"$1\"";

/// Prints a well-formed ready line naming the shell itself.
const PRINT_READY_LINE: &str =
    "echo \"bookrack daemon running: pid=$$ mcp=127.0.0.1:1 control_sock=$2\"";

/// A lock left behind by an earlier daemon names another pid. A child
/// that prints its ready line while that lock is still the one on disk
/// is not ready: a client discovering the daemon through the lock
/// would reach the leftover, not this child.
#[tokio::test]
async fn a_leftover_lock_is_not_the_childs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let sock = dir.path().join("control.sock");
    std::fs::write(&lock, "pid=4294967295\nmcp=127.0.0.1:0\n").expect("write a leftover lock");
    let child = sh_with(
        &format!("{PRINT_READY_LINE}; exec sleep 30"),
        &[lock.as_os_str(), sock.as_os_str()],
    );

    let failure = match child.wait_ready(&lock, Duration::from_secs(5)).await {
        Err(failure) => failure,
        Ok(_) => panic!("a lock naming another pid was read as this child being ready"),
    };
    assert!(
        failure.why.contains("names pid 4294967295"),
        "the failure must name the leftover lock's pid: {failure}",
    );
}

/// `bookrack run` takes the session lock before it binds MCP, so a
/// held MCP address refuses the bring-up with the lock file already
/// written. The child's exit is the answer, and the failure carries
/// the daemon's own refusal.
#[tokio::test]
async fn a_daemon_that_exits_after_taking_the_lock_is_not_ready() {
    let sandbox = Sandbox::new();
    let occupant = std::net::TcpListener::bind("127.0.0.1:0").expect("hold an address");
    let addr = occupant.local_addr().expect("held address").to_string();
    let mut cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    cmd.args(["run", "--mcp-addr", &addr]);
    let daemon = DaemonProcess::spawn(cmd).expect("spawn bookrack run");

    let failure = match daemon
        .wait_ready(&sandbox.tty_lock_path(), Duration::from_secs(20))
        .await
    {
        Err(failure) => failure,
        Ok(_) => panic!("a daemon that refused its bring-up was read as ready"),
    };
    assert!(
        failure.why.starts_with("the child exited"),
        "the failure must say the child exited: {failure}",
    );
    assert!(
        failure.stderr.contains("cannot serve MCP on"),
        "the failure must carry the daemon's own refusal: {failure}",
    );
    let lock = std::fs::read_to_string(sandbox.tty_lock_path())
        .expect("premise: a refused bring-up leaves its lock file behind");
    assert!(
        lock.contains("mcp="),
        "premise: the leftover lock carries the marker a lock-based wait accepts: {lock}",
    );
}

/// The control case: a daemon that comes up is ready inside its
/// budget, and a client started the moment the wait returns is served.
#[tokio::test]
async fn a_daemon_that_comes_up_is_ready() {
    let sandbox = Sandbox::new();
    let mut cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    cmd.arg("run");
    let daemon = DaemonProcess::spawn(cmd)
        .expect("spawn bookrack run")
        .wait_ready(&sandbox.tty_lock_path(), Duration::from_secs(20))
        .await
        .expect("a daemon that comes up must read as ready");

    let output = Command::from(bookrack_cmd!(&sandbox).build())
        .arg("quit")
        .output()
        .await
        .expect("run bookrack quit");
    assert!(
        output.status.success(),
        "bookrack quit failed right after the wait: status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let exit = daemon
        .exit_within(Duration::from_secs(5))
        .await
        .expect("daemon must exit within 5 s of `bookrack quit`");
    assert!(
        exit.status.success(),
        "daemon exited non-zero: {:?}",
        exit.status
    );
}

/// A child that took the lock and is alive, but has not printed its
/// ready line, is not ready: bring-up has not returned. A wait that
/// accepts the lock once the child is still alive reads it as ready.
#[tokio::test]
async fn a_live_child_without_a_ready_line_is_not_ready() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let sock = dir.path().join("control.sock");
    let child = sh_with(
        &format!("{WRITE_OWN_LOCK}; exec sleep 30"),
        &[lock.as_os_str(), sock.as_os_str()],
    );

    let failure = match child.wait_ready(&lock, Duration::from_secs(2)).await {
        Err(failure) => failure,
        Ok(_) => panic!("a live child that never printed its ready line was read as ready"),
    };
    assert!(
        failure
            .why
            .starts_with("the child printed no ready line within 2s"),
        "the failure must name the missing ready line and the budget: {failure}",
    );
}

/// A ready line is necessary, not sufficient: the control socket must
/// answer too. Here it accepts connections and never replies, so the
/// wait must give up inside its own budget rather than hang on the
/// call.
#[tokio::test]
async fn a_ready_line_without_a_control_plane_answer_is_not_ready() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("session.lock");
    let sock = dir.path().join("control.sock");
    let _silent = UnixListener::bind(&sock).expect("bind a socket that never answers");
    let child = sh_with(
        &format!("{WRITE_OWN_LOCK}; {PRINT_READY_LINE}; exec sleep 30"),
        &[lock.as_os_str(), sock.as_os_str()],
    );

    let budget = Duration::from_secs(3);
    let started = Instant::now();
    let failure = match child.wait_ready(&lock, budget).await {
        Err(failure) => failure,
        Ok(_) => panic!("a ready line whose control socket never answered was read as ready"),
    };
    let elapsed = started.elapsed();
    assert!(
        failure.why.contains("daemon.version"),
        "the failure must name the unanswered call: {failure}",
    );
    assert!(
        elapsed < budget + Duration::from_secs(2),
        "the wait must end inside its budget, took {elapsed:?}: {failure}",
    );
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
