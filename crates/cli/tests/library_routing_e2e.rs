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
//! A selection given as a path is sugared into the registry name that
//! claims that root, so `--data-dir` and `BOOKRACK_DATA_DIR` reach the
//! same library `--library` does. A root no entry claims has no name to
//! send, so the daemon is asked whether it is the root it serves: the
//! ordinary single-library setup answers yes and proceeds unnamed —
//! `rpc_e2e` runs on exactly that shape — and a daemon serving
//! something else refuses rather than acting on its own default.
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

/// Both path channels reach the library that owns the root, and a root
/// this daemon does not serve is refused instead of being answered by
/// its default.
///
/// The three cases share one daemon and differ only in how the caller
/// names the library, which is what makes them comparable: `--data-dir`
/// and the environment variable must land where `--library beta` lands,
/// and a stranger root must land nowhere.
#[tokio::test]
async fn a_path_selection_reaches_the_library_that_owns_the_root() {
    let sandbox = two_library_world();
    let lock_path = sandbox.tty_lock_path();
    let beta_root = sandbox.data_root("beta-root");

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

    let by_flag = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--data-dir", &beta_root.display().to_string()])
        .args(["rpc", "call", "library.info", "{}"])
        .output()
        .await
        .expect("run bookrack rpc call");
    let stdout = String::from_utf8_lossy(&by_flag.stdout).into_owned();
    assert!(
        by_flag.status.success(),
        "a registered root must route: stderr={}",
        String::from_utf8_lossy(&by_flag.stderr),
    );
    assert!(
        stdout.contains("\"library_name\": \"beta\""),
        "--data-dir must reach the library that owns the root: {stdout}",
    );

    let by_env = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .data_dir(beta_root.as_path())
            .build(),
    )
    .args(["rpc", "call", "library.info", "{}"])
    .output()
    .await
    .expect("run bookrack rpc call");
    let stdout = String::from_utf8_lossy(&by_env.stdout).into_owned();
    assert!(
        by_env.status.success(),
        "the environment variable is the same selection: stderr={}",
        String::from_utf8_lossy(&by_env.stderr),
    );
    assert!(
        stdout.contains("\"library_name\": \"beta\""),
        "BOOKRACK_DATA_DIR must reach the same library the flag does: {stdout}",
    );

    let stranger = sandbox.data_root("stranger-root");
    let unclaimed = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--data-dir", &stranger.display().to_string()])
        .args(["rpc", "call", "library.info", "{}"])
        .output()
        .await
        .expect("run bookrack rpc call");
    let stderr = String::from_utf8_lossy(&unclaimed.stderr).into_owned();
    assert_eq!(
        unclaimed.status.code(),
        Some(2),
        "a root this daemon does not serve is caller input: stderr={stderr}",
    );
    assert!(
        stderr.contains(&stranger.display().to_string()),
        "the refusal must name the root that was asked for: {stderr}",
    );
    assert!(
        stderr.contains(&sandbox.data_root("alpha-root").display().to_string()),
        "and the one the daemon serves, so both sides are on screen: {stderr}",
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

/// A locally resolving command keeps a path selection as what it is
/// there: a switch into that root. Nothing about routing reaches it,
/// and an unregistered root is none of the registry's business.
///
/// This case comes from the pre-flight suite, which retired with the
/// check it tested. What it pins is the other side of the split that
/// outlived it.
#[tokio::test]
async fn a_local_command_still_switches_roots_by_path() {
    let sandbox = two_library_world();
    let stranger = sandbox.data_root("stranger-root");

    let out = Command::from(bookrack_cmd!(&sandbox).without_data_dir().build())
        .args(["--data-dir", &stranger.display().to_string()])
        .args(["retrieval", "list"])
        .output()
        .await
        .expect("run bookrack retrieval list");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a local command must reach its own resolution: stderr={stderr}",
    );
    assert!(
        stdout.contains("No retrieval calls."),
        "it must report the empty root it was pointed at: stdout={stdout} stderr={stderr}",
    );
}
