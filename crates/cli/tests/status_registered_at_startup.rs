// SPDX-License-Identifier: Apache-2.0

//! `bookrack status` on a daemon that registered its root at bring-up.
//!
//! The card's `registered_at_startup` row is the operator-facing end of
//! auto-registration: a `bookrack run` on a manifest-bearing root the
//! registry did not know writes the entry and the next `status` says so.
//! The row is absent again on a daemon that had nothing to register.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::time::Duration;

use bookrack_config::{LibraryKind, new_manifest, write_manifest};
use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

/// `bookrack --json status` against the given sandbox, parsed.
async fn status_card(sandbox: &Sandbox) -> serde_json::Value {
    let output = Command::from(bookrack_cmd!(sandbox).without_data_dir().build())
        .args(["--json", "status"])
        .output()
        .await
        .expect("run bookrack status");
    assert!(
        output.status.success(),
        "status must answer on a healthy daemon: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).expect("status --json is JSON")
}

async fn stop(daemon: DaemonProcess) {
    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn the_card_names_the_root_bring_up_registered() {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
    let gamma = sandbox.data_root("gamma-root");
    write_manifest(&gamma, &new_manifest("gamma", LibraryKind::Test, None))
        .expect("write the gamma manifest");

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::url())
            .build(),
    );
    daemon_cmd.args(["--data-dir", &gamma.display().to_string(), "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&sandbox.tty_lock_path(), Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    let card = status_card(&sandbox).await;
    assert_eq!(
        card["library"]["registered_at_startup"], "gamma",
        "the card must name what bring-up registered: {card}",
    );
    assert_eq!(card["library"]["name"], "gamma", "{card}");
    let registry = std::fs::read_to_string(sandbox.registry_path()).expect("read registry");
    assert!(
        registry.contains("[libraries.gamma]") && registry.contains("default = \"alpha\""),
        "the entry is written and the default untouched: {registry}",
    );

    stop(daemon).await;
}

#[tokio::test]
async fn the_card_has_no_such_row_when_bring_up_registered_nothing() {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::url())
            .build(),
    );
    daemon_cmd.args(["--library", "alpha", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&sandbox.tty_lock_path(), Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    let card = status_card(&sandbox).await;
    assert!(
        card["library"].get("registered_at_startup").is_none(),
        "a registry-selected daemon registered nothing: {card}",
    );

    stop(daemon).await;
}
