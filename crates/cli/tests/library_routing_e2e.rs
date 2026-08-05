// SPDX-License-Identifier: Apache-2.0

//! `--library` against a daemon that serves more than one library.
//!
//! A daemon mounts every registered library at bring-up, so naming one
//! of them is a routing decision the daemon can honour. Until the
//! selection travelled with the call it was compared against the one
//! name the session lock records — the library the daemon came up
//! under — and every other mounted library was unreachable from the
//! CLI, with `bookrack rpc call` carrying no selection at all as the
//! only way in.
//!
//! Both directions are asserted from one live daemon: a name it serves
//! answers for that library, and a name the registry does not hold is
//! refused as caller input rather than quietly answered by the
//! default.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::time::Duration;

use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

/// A registry naming two libraries, with `alpha` as the default and
/// the library the daemon is started under.
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

#[tokio::test]
async fn a_library_the_daemon_serves_answers_for_itself() {
    let sandbox = two_library_world();
    let lock_path = sandbox.tty_lock_path();

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::url())
            .build(),
    );
    daemon_cmd.args(["--library", "alpha", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&lock_path, Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    // `library.info` names the library it answered for, which is what
    // makes this test discriminate: two empty roots report identical
    // counts, and only the identity says which one was read.
    let output = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--library", "beta", "rpc", "call", "library.info", "{}"])
        .output()
        .await
        .expect("run bookrack rpc call");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "a mounted library must be reachable by name: status={:?} stderr={stderr}",
        output.status,
    );
    assert!(
        stdout.contains("\"library_name\": \"beta\""),
        "the answer must come from beta, not from the library the daemon came up under: {stdout}",
    );

    // The other direction: an unknown name is caller input, not a
    // silent fallback to the default. Without this a client that
    // dropped the selection entirely would pass the assertion above.
    let output = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--library", "nosuch", "rpc", "call", "library.info", "{}"])
        .output()
        .await
        .expect("run bookrack rpc call");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unknown library is caller input: stderr={stderr}",
    );
    assert!(
        stderr.contains("nosuch"),
        "the refusal must name the library it could not resolve: {stderr}",
    );

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(5)).await;
}
