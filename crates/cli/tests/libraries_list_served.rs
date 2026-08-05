// SPDX-License-Identifier: Apache-2.0

//! `bookrack libraries list` reports the registry, and marks which of
//! its entries a running daemon actually holds.
//!
//! The two facts come from different places — the entries from the
//! registry file, the served set from the daemon — and the listing has
//! to keep them apart. Three shapes cover that:
//!
//! * a daemon whose primary is registered mounts every entry, so every
//!   row is served;
//! * a daemon started on a root no entry claims mounts that root alone,
//!   so **no** row is served even though the registry is unchanged —
//!   this is the case an implementation that copied `is_default`, or
//!   the entry list itself, into the column would fail;
//! * with no daemon the column is absent, rather than a column of noes:
//!   nobody was asked, which is not the same as nobody serving them.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::time::Duration;

use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

/// A registry naming two libraries, `alpha` the default.
fn two_library_world() -> Sandbox {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    sandbox
}

/// `bookrack --json libraries list` against the given sandbox.
async fn listing(sandbox: &Sandbox) -> String {
    let output = Command::from(bookrack_cmd!(sandbox).without_data_dir().build())
        .args(["--json", "libraries", "list"])
        .output()
        .await
        .expect("run bookrack libraries list");
    assert!(
        output.status.success(),
        "the listing reads the registry and must not need a daemon: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
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
async fn every_entry_is_served_when_the_daemon_came_up_through_the_registry() {
    let sandbox = two_library_world();
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

    let stdout = listing(&sandbox).await;
    assert_eq!(
        stdout.matches("\"served\":true").count(),
        2,
        "a registry-selected daemon mounts every entry: {stdout}",
    );

    stop(daemon).await;
}

#[tokio::test]
async fn no_entry_is_served_when_the_daemon_holds_a_root_the_registry_does_not_name() {
    let sandbox = two_library_world();
    let stranger = sandbox.data_root("stranger-root");
    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::url())
            .build(),
    );
    daemon_cmd.args(["--data-dir", &stranger.display().to_string(), "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&sandbox.tty_lock_path(), Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    let stdout = listing(&sandbox).await;
    // The registry is the same file as in the test above; only the
    // daemon differs. A column derived from the entries rather than
    // from the daemon would be `true` here.
    assert_eq!(
        stdout.matches("\"served\":false").count(),
        2,
        "a daemon on an unregistered root serves no registered library: {stdout}",
    );
    assert!(
        !stdout.contains("\"served\":true"),
        "no entry may be marked served: {stdout}",
    );

    stop(daemon).await;
}

#[tokio::test]
async fn the_served_column_is_absent_when_no_daemon_answers() {
    let sandbox = two_library_world();
    let stdout = listing(&sandbox).await;
    assert!(
        stdout.contains("\"name\":\"alpha\""),
        "the registry is still listed without a daemon: {stdout}",
    );
    assert!(
        !stdout.contains("served"),
        "with nobody asked, the column must be absent rather than false: {stdout}",
    );
}
