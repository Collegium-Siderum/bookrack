// SPDX-License-Identifier: Apache-2.0

//! End-to-end round-trip for `bookrack rpc call library.info` against a
//! live `bookrack run` daemon.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

use std::time::Duration;

use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

#[tokio::test]
async fn library_info_round_trips_through_running_daemon() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");

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

    let exit = daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon must exit within 5 s of SIGTERM");
    let (status, stderr) = (exit.status, exit.stderr);
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

/// An `--audit-profile` naming no built-in used to run the command to
/// completion under a different profile and exit `0`. It is now
/// caller input, which the exit-code table puts at `2`.
///
/// The exit code alone is a weak signal here — several unrelated
/// failures also exit `2` — so the assertion that discriminates is
/// stderr naming the profile that was refused.
#[tokio::test]
async fn an_unknown_audit_profile_exits_two_from_the_cli() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();
    // An empty directory: with the name accepted, `dryrun` fails for
    // having nothing to scan, which is a different exit code from the
    // refusal under test.
    let scan_dir = sandbox.path().join("empty-scan");
    std::fs::create_dir_all(&scan_dir).expect("create scan dir");

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");

    let output = Command::from(bookrack_cmd!(&sandbox).build())
        .arg("--audit-profile")
        .arg("strictt")
        .arg("dryrun")
        .arg(&scan_dir)
        .output()
        .await
        .expect("run bookrack dryrun");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unknown audit profile is caller input: stderr={stderr}",
    );
    assert!(
        stderr.contains("strictt"),
        "the refusal must name the profile it rejected: stderr={stderr}",
    );

    daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon stops on SIGTERM");
}

/// `remove` against an id the library does not hold is caller input,
/// which the exit-code table puts at `2`. It exited `1` while the
/// refusal reached the CLI as an internal error.
///
/// Driven through the binary rather than through `from_rpc`, because
/// the classification is only half of what produces the exit code.
#[tokio::test]
async fn a_remove_of_an_unknown_book_exits_two_from_the_cli() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");

    let output = Command::from(bookrack_cmd!(&sandbox).build())
        .arg("remove")
        .arg("999999")
        .arg("--yes")
        .output()
        .await
        .expect("run bookrack remove");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(2),
        "a remove the library cannot satisfy is caller input: stderr={stderr}",
    );

    daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon stops on SIGTERM");
}

/// `daemon.config` answers for the process and for every library it
/// serves, so it carries no selection: without one the call succeeds
/// and reports the process rows, and a `--library` flag is refused by
/// the client rather than sent and left unread.
#[tokio::test]
async fn rpc_call_daemon_config_refuses_a_library_flag() {
    let sandbox = Sandbox::new();
    let lock_path = sandbox.tty_lock_path();
    let alpha = sandbox.data_root("alpha-root");
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .extra_env("BOOKRACK_EMBED_BATCH_MAX_CHUNKS", "7")
            .extra_env("BOOKRACK_NOT_A_KNOB", "1")
            .build(),
    );
    daemon_cmd.args(["--library", "alpha", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");

    let plain = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["rpc", "call", "daemon.config"])
        .output()
        .await
        .expect("run bookrack rpc call");
    assert!(
        plain.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let config: serde_json::Value =
        serde_json::from_slice(&plain.stdout).expect("daemon.config prints JSON");
    // The daemon's own environment, not this caller's.
    let chunks = config["rows"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["key"] == "embed.batch_max_chunks")
        })
        .expect("embed.batch_max_chunks row");
    assert_eq!(chunks["value"], "7", "{chunks}");
    assert_eq!(config["mounted_at_start"], serde_json::json!(["alpha"]));
    // The binary supplies its inventory, so the section is computed.
    let unclaimed: Vec<&str> = config["unclaimed_variables"]
        .as_array()
        .expect("unclaimed_variables is computed under the binary")
        .iter()
        .filter_map(|var| var["key"].as_str())
        .collect();
    assert_eq!(unclaimed, ["BOOKRACK_NOT_A_KNOB"], "{config}");

    let flagged = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--library", "alpha", "rpc", "call", "daemon.config"])
        .output()
        .await
        .expect("run bookrack rpc call");
    let stderr = String::from_utf8_lossy(&flagged.stderr);
    assert_eq!(flagged.status.code(), Some(2), "stderr={stderr}");
    assert!(
        stderr.contains("cannot run this command against library 'alpha'"),
        "{stderr}"
    );

    daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon stops on SIGTERM");
}
