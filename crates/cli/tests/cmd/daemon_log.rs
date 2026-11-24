// SPDX-License-Identifier: Apache-2.0

//! What the daemon log holds of bring-up: every event from the session
//! lock on reaches the file, the retired-variable warning included.
//!
//! The log is read after a `bookrack quit`, once the process has exited
//! on its own: the non-blocking writer flushes when its guard drops, so
//! a read before the exit could miss lines that are on their way.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

/// Every JSON event in the files under `logs`, in file-name order.
fn log_events(logs: &Path) -> Vec<Value> {
    let mut files: Vec<_> = std::fs::read_dir(logs)
        .unwrap_or_else(|e| panic!("read the daemon log directory {}: {e}", logs.display()))
        .map(|entry| entry.expect("read a log directory entry").path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
                .lines()
                .map(|line| {
                    serde_json::from_str::<Value>(line)
                        .unwrap_or_else(|e| panic!("a log line is not JSON ({e}): {line}"))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn message(event: &Value) -> &str {
    event["fields"]["message"].as_str().unwrap_or_default()
}

fn rendered(events: &[Value]) -> String {
    events
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_retired_variable_is_logged_when_the_daemon_starts() {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
    let lock_path = sandbox.tty_lock_path();

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .extra_env("BOOKRACK_SEARCH_TOP_K", "5")
            .build(),
    );
    daemon_cmd.args(["--library", "alpha", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");

    let quit = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .arg("quit")
        .output()
        .await
        .expect("run bookrack quit");
    assert!(
        quit.status.success(),
        "bookrack quit failed: status={:?}, stderr={}",
        quit.status,
        String::from_utf8_lossy(&quit.stderr),
    );
    let exit = daemon
        .exit_within(Duration::from_secs(5))
        .await
        .expect("the daemon exits after `bookrack quit`");
    assert!(
        exit.status.success(),
        "daemon exited non-zero: status={:?}, stderr={}",
        exit.status,
        exit.stderr,
    );

    let events = log_events(&sandbox.daemon_state_dir().join("logs"));
    let retired: Vec<&Value> = events
        .iter()
        .filter(|event| message(event) == "a retired environment variable is set and ignored")
        .collect();
    assert!(
        !retired.is_empty(),
        "no retired-variable warning in the daemon log:\n{}",
        rendered(&events)
    );
    assert!(
        retired.iter().any(|event| {
            event["level"] == "WARN" && event["fields"]["variable"] == "BOOKRACK_SEARCH_TOP_K"
        }),
        "the retired-variable warning names the variable at WARN: {retired:?}"
    );
    for bring_up in [
        "bookrack session lock acquired",
        "control plane socket bound",
        "MCP listener bound",
    ] {
        assert!(
            events.iter().any(|event| message(event) == bring_up),
            "no {bring_up:?} line in the daemon log:\n{}",
            rendered(&events)
        );
    }
}
