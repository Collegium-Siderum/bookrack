// SPDX-License-Identifier: Apache-2.0

//! `bookrack libraries default` and the daemon serving the library
//! agree afterwards.
//!
//! The pointer has two homes: the registry file on disk, and the cache
//! a running daemon seeded from it at bring-up. A CLI that wrote only
//! the file left the two disagreeing — `libraries list` and the status
//! card would report the new default while unnamed calls kept reaching
//! the old one, which is three surfaces and two answers.
//!
//! The discriminating question is the third one: whether an *unnamed*
//! call lands on the new default. The first two read the registry, so
//! an implementation that only writes the file satisfies both.
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

async fn run(sandbox: &Sandbox, args: &[&str]) -> std::process::Output {
    Command::from(bookrack_cmd!(sandbox).without_data_dir().build())
        .args(args)
        .output()
        .await
        .expect("run bookrack")
}

/// The name of the row `libraries list --json` marks as the registry
/// default, read as JSON so the assertion does not depend on field
/// order in the serialised form.
fn default_row(listing: &str) -> Option<&str> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(listing).ok()?;
    let name = rows
        .into_iter()
        .find(|row| row["is_default"] == serde_json::Value::Bool(true))?
        .get("name")?
        .as_str()?
        .to_string();
    // Borrowed back out of the input so the caller keeps a `&str`
    // without the test owning a second copy of the name.
    listing
        .match_indices(&name)
        .next()
        .map(|(at, _)| &listing[at..at + name.len()])
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
async fn the_daemon_follows_a_default_moved_from_the_cli() {
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

    let moved = run(&sandbox, &["libraries", "default", "beta"]).await;
    assert!(
        moved.status.success(),
        "moving the default failed: status={:?} stderr={}",
        moved.status,
        String::from_utf8_lossy(&moved.stderr),
    );

    // 1. The registry file: what the listing reads.
    let listing = run(&sandbox, &["--json", "libraries", "list"]).await;
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    assert!(
        default_row(&listing) == Some("beta"),
        "the registry listing does not mark beta as the default: {listing}",
    );

    // 2. The daemon's own report of the served set. It answers from
    //    the cache, not from the file, so this is already more than a
    //    second reading of the registry.
    let status = run(&sandbox, &["--json", "status"]).await;
    let status = String::from_utf8_lossy(&status.stdout).into_owned();
    let card: serde_json::Value =
        serde_json::from_str(&status).expect("status --json is a JSON document");
    let served_default = card["library"]["served"]
        .as_array()
        .expect("the card carries a served set")
        .iter()
        .find(|row| row["default"] == serde_json::Value::Bool(true))
        .and_then(|row| row["name"].as_str());
    assert_eq!(
        served_default,
        Some("beta"),
        "the daemon's served set still marks another library as the default: {status}",
    );

    // 3. The one that discriminates: an unnamed call, which the daemon
    //    routes through its own default pointer. A CLI that wrote the
    //    registry and told nobody leaves this one on alpha.
    let info = run(&sandbox, &["--json", "libraries", "info"]).await;
    assert!(
        info.status.success(),
        "the unnamed info call failed: status={:?} stderr={}",
        info.status,
        String::from_utf8_lossy(&info.stderr),
    );
    let info = String::from_utf8_lossy(&info.stdout).into_owned();
    assert!(
        info.contains("beta-root"),
        "the daemon still routes an unnamed call to the old default \
         (the registry says beta, the daemon answers from another root): {info}",
    );
    assert!(
        !info.contains("alpha-root"),
        "the unnamed call reached the old default: {info}",
    );

    stop(daemon).await;
}

/// With nothing listening the verb keeps its offline behaviour: the
/// registry is written and the pointer survives to the next start.
#[tokio::test]
async fn with_no_daemon_the_registry_is_still_written() {
    let sandbox = two_library_world();
    let moved = run(&sandbox, &["libraries", "default", "beta"]).await;
    assert!(
        moved.status.success(),
        "the verb has to work with no daemon: status={:?} stderr={}",
        moved.status,
        String::from_utf8_lossy(&moved.stderr),
    );

    let listing = run(&sandbox, &["--json", "libraries", "list"]).await;
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    assert!(
        default_row(&listing) == Some("beta"),
        "the offline write did not move the pointer: {listing}",
    );
}
