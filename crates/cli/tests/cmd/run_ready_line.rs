// SPDX-License-Identifier: Apache-2.0

//! The `bookrack run` ready output when bring-up skipped a library.
//!
//! The first line is the ready line tooling has always matched, byte
//! for byte in shape; what bring-up did not mount is said after it,
//! never in it.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

use std::time::Duration;

use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

use crate::common::DaemonProcess;

/// Whether `line` is exactly `bookrack daemon running: pid=<digits>
/// mcp=<word> control_sock=<word>`.
fn is_ready_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("bookrack daemon running: pid=") else {
        return false;
    };
    let mut fields = rest.split(' ');
    let pid_ok = fields
        .next()
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()));
    let mcp_ok = fields
        .next()
        .and_then(|f| f.strip_prefix("mcp="))
        .is_some_and(|v| !v.is_empty());
    let sock_ok = fields
        .next()
        .and_then(|f| f.strip_prefix("control_sock="))
        .is_some_and(|v| !v.is_empty());
    pid_ok && mcp_ok && sock_ok && fields.next().is_none()
}

#[tokio::test]
async fn the_ready_line_is_unchanged_and_skipped_libraries_follow_it() {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let _held = bookrack_session::RootLock::acquire(&beta, std::process::id(), "test")
        .expect("hold the beta root");
    let lock_path = sandbox.tty_lock_path();

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.args(["--library", "alpha", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");
    let output = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .arg("quit")
        .output()
        .await
        .expect("run bookrack quit");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let exit = daemon
        .exit_within(Duration::from_secs(5))
        .await
        .expect("daemon exits after quit");
    let stdout = exit.stdout;
    let mut lines = stdout.lines();
    let first = lines.next().unwrap_or_default();
    assert!(
        is_ready_line(first),
        "the first line is the ready line, whole: {first:?}"
    );
    let rest: Vec<&str> = lines.collect();
    assert!(
        rest.iter()
            .any(|line| line.starts_with("not mounted: beta")),
        "the skipped library follows the ready line: {stdout}"
    );
    assert!(
        rest.iter()
            .any(|line| line.contains("bookrack libraries list")),
        "the output points at the listing for the reason: {stdout}"
    );
}
