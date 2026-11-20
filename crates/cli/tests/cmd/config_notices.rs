// SPDX-License-Identifier: Apache-2.0

//! A `config.toml` key this build does not resolve is reported, never
//! refused: the library still mounts, and each surface that reads the
//! file names the key.
//!
//! The daemon embeds through the in-process [`EmbedStub`], so no Ollama
//! daemon is required.

#![cfg(unix)]

use std::process::Output;
use std::time::Duration;

use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

/// A registry naming two libraries, `alpha` the default, with `beta`'s
/// `config.toml` holding `body`.
fn two_library_world(body: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    std::fs::write(beta.join("config.toml"), body).expect("write beta's config.toml");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    sandbox
}

async fn bookrack(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::from(bookrack_cmd!(sandbox).without_data_dir().build())
        .args(args)
        .output()
        .await
        .expect("run bookrack")
}

#[tokio::test]
async fn a_library_with_a_retired_key_still_mounts_and_reports_it() {
    let sandbox = two_library_world("log_directive = \"debug\"\n");
    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.args(["--library", "alpha", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&sandbox.tty_lock_path(), Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");

    let listing = bookrack(&sandbox, &["--json", "libraries", "list"]).await;
    let stdout = String::from_utf8_lossy(&listing.stdout);
    assert_eq!(
        stdout.matches("\"served\":true").count(),
        2,
        "a retired key must not keep its library from mounting: {stdout}",
    );

    let effective = bookrack(
        &sandbox,
        &["--library", "beta", "--json", "config", "effective"],
    )
    .await;
    let report: serde_json::Value =
        serde_json::from_slice(&effective.stdout).expect("config effective prints JSON");
    let notices = report["config_notices"]
        .as_array()
        .expect("config_notices is an array");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["kind"], "retired");
    assert_eq!(notices[0]["key"], "log_directive");

    let dump = bookrack(&sandbox, &["libraries", "config", "beta"]).await;
    let notes = String::from_utf8_lossy(&dump.stderr);
    assert!(
        notes.contains("note: `log_directive` is retired"),
        "the dump annotates the retired line: {notes}",
    );

    daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon stops on SIGTERM");
}

#[tokio::test]
async fn a_notice_never_carries_the_value_it_was_read_with() {
    let sandbox = two_library_world("mcp_addr = \"secret-host:1\"\ncolour = 1\n");

    let effective = bookrack(
        &sandbox,
        &["--library", "beta", "--json", "config", "effective"],
    )
    .await;
    let stdout = String::from_utf8_lossy(&effective.stdout);
    assert!(
        stdout.contains("\"mcp_addr\""),
        "the key is named: {stdout}"
    );
    assert!(!stdout.contains("secret-host"), "{stdout}");

    let dump = bookrack(&sandbox, &["--json", "libraries", "config", "beta"]).await;
    let stdout = String::from_utf8_lossy(&dump.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("libraries config --json prints JSON");
    let keys: Vec<&str> = parsed["notices"]
        .as_array()
        .expect("notices is an array")
        .iter()
        .map(|notice| notice["key"].as_str().expect("key"))
        .collect();
    assert_eq!(keys, ["colour", "mcp_addr"]);
    assert!(!stdout.contains("secret-host"), "{stdout}");

    // A file with nothing to report still carries the array.
    let clean = bookrack(&sandbox, &["--json", "libraries", "config", "alpha"]).await;
    let parsed: serde_json::Value =
        serde_json::from_slice(&clean.stdout).expect("libraries config --json prints JSON");
    assert_eq!(parsed["notices"], serde_json::json!([]));
}
