// SPDX-License-Identifier: Apache-2.0

//! Runtime mounting: a daemon that came up on one library starts
//! serving a second one without a restart.
//!
//! An eager daemon mounts the whole registry at bring-up, so the state
//! this exercises — registered but not mounted — can only be built
//! afterwards: seed a one-entry registry, start the daemon, then rewrite
//! the registry with the second entry and mount it over the control
//! socket.
//!
//! The embedder probe each mount performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{Result, eyre};
use serde_json::Value;

use crate::common::{connect, join_with_deadline, recv, send};

/// Isolate the process and seed a registry the daemon will come up on.
/// Only `alpha` is registered at bring-up; the second root exists on
/// disk but has no entry yet.
fn world() -> (&'static Sandbox, PathBuf, PathBuf) {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    initialize(&alpha);
    initialize(&beta);
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
    (sandbox, alpha, beta)
}

/// Give a root the catalogs a real library carries, so `library.fork`
/// has something to clone. Bring-up creates them lazily; fork refuses
/// a source that has none.
fn initialize(root: &Path) {
    for db in ["catalog.db", "papers_catalog.db"] {
        bookrack_catalog::Catalog::open(&root.join(db)).expect("seed catalog");
    }
    for db in ["corpus.db", "papers_corpus.db"] {
        bookrack_corpus::Corpus::open(&root.join(db)).expect("seed corpus");
    }
}

/// Add `beta` to the on-disk registry after the daemon is already up.
fn register_beta(sandbox: &Sandbox, alpha: &Path, beta: &Path) {
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha), ("beta", beta)]);
}

async fn start(runtime_root: &Path) -> Result<DaemonRuntime> {
    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.to_path_buf());
    DaemonRuntime::start(opts).await
}

/// The library set a daemon serves is decided at bring-up today; this
/// is the assertion that it stops being decided there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mount_serves_a_library_registered_after_bring_up() -> Result<()> {
    let (sandbox, alpha, beta) = world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(runtime_root.path()).await?;
    register_beta(sandbox, &alpha, &beta);

    // The daemon came up before the entry existed, so it is not serving
    // it — without this the mount below could be a no-op and still pass.
    assert!(
        runtime.registry.get(Some("beta")).is_err(),
        "the fixture has to start with beta registered but unmounted",
    );

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.mount","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert!(resp["error"].is_null(), "mount was refused: {resp}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let mut names = library_names(&resp)?;
        names.sort_unstable();
        assert_eq!(names, ["alpha", "beta"], "{resp}");

        // Listed is not served: a read addressed to the new library has
        // to route to its own handle, which is what an open mount buys.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":3,"method":"library.info","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert!(
            resp["error"].is_null(),
            "the mounted library does not answer a read: {resp}"
        );
        let root = resp["result"]["data_dir"]
            .as_str()
            .ok_or_else(|| eyre!("library.info carries no data_dir: {resp}"))?;
        assert!(
            root.ends_with("beta-root"),
            "the read routed to another library's root: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// Mounting takes a registry name, so a name the registry does not
/// carry is caller input, not a fault.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mount_refuses_a_name_the_registry_does_not_know() -> Result<()> {
    let (_sandbox, _alpha, _beta) = world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(runtime_root.path()).await?;

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.mount","params":{"name":"ghost"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["error"]["code"], -32010, "{resp}");
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("ghost"),
            "the refusal has to name what was asked for: {resp}"
        );

        // A refused mount leaves the served set alone.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(library_names(&resp)?, ["alpha"], "{resp}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// Two registry names on one root is a registry to fix. Bring-up
/// already refuses it; a runtime mount that did not would let the same
/// machine reach a state its own startup rejects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mount_refuses_a_root_another_mount_already_claims() -> Result<()> {
    let (sandbox, alpha, _beta) = world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(runtime_root.path()).await?;
    // `twin` names the root `alpha` is already served from.
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("twin", alpha.as_path())],
    );

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.mount","params":{"name":"twin"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let message = resp["error"]["message"].as_str().unwrap_or_default();
        assert!(
            !resp["error"].is_null(),
            "the duplicate root was served: {resp}"
        );
        assert!(
            message.contains("data root"),
            "the refusal has to say the root is the problem, not the name: {resp}"
        );
        // Not the "no such library" refusal — the name resolves fine.
        assert_ne!(resp["error"]["code"], -32010, "{resp}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(library_names(&resp)?, ["alpha"], "{resp}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// The point of unmounting: the root goes back. An implementation that
/// only takes the name out of the map passes every listing assertion
/// and fails this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmount_releases_the_data_root_lock() -> Result<()> {
    use bookrack_session::{RootLock, is_root_lock_conflict};

    let (sandbox, alpha, beta) = world();
    let runtime_root = tempfile::tempdir()?;
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let runtime = start(runtime_root.path()).await?;

    // Held while served, so the release below is a change of state and
    // not a root that was never locked.
    let held = RootLock::acquire(&beta, std::process::id(), "test");
    assert!(
        held.as_ref().err().is_some_and(is_root_lock_conflict),
        "beta's root has to be locked while the daemon serves it",
    );
    drop(held);

    let sock = runtime.control_sock.path.clone();
    let beta_for_driver = beta.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.unmount","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert!(resp["error"].is_null(), "unmount was refused: {resp}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(library_names(&resp)?, ["alpha"], "{resp}");

        // The whole point: this process can now take the root.
        RootLock::acquire(&beta_for_driver, std::process::id(), "test")
            .map_err(|e| eyre!("the unmounted root is still locked: {e:#}"))?;

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// Unmounting a library with work still queued against it would let
/// each of those jobs fail on its next pull — silently burning work the
/// operator submitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmount_refuses_a_library_with_queued_work() -> Result<()> {
    let (sandbox, alpha, beta) = world();
    let runtime_root = tempfile::tempdir()?;
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        // Pause first so the job stays `Pending` instead of being
        // pulled and finished before the unmount is attempted.
        send(&mut w, r#"{"jsonrpc":"2.0","id":1,"method":"queue.pause"}"#).await?;
        let _ = recv(&mut reader).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"ingest.submit","params":{"paths":["/tmp/unmount-fixture.txt"],"library":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert!(resp["result"]["job_ids"].is_array(), "{resp}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":3,"method":"library.unmount","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert!(
            !resp["error"].is_null(),
            "queued work did not stop the unmount: {resp}"
        );
        let message = resp["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("queued work"),
            "the refusal has to name the queue as the reason: {resp}"
        );
        // Not the "unknown library" refusal dressed up as a reason: an
        // implementation that reports every failed unmount that way
        // would satisfy a bare is-error assertion.
        assert!(
            !message.contains("no library named"),
            "the library is served; the refusal must not claim otherwise: {resp}"
        );

        // Still served — a refused unmount changes nothing.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":4,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let mut names = library_names(&resp)?;
        names.sort_unstable();
        assert_eq!(names, ["alpha", "beta"], "{resp}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// The two libraries a daemon cannot stop serving without contradicting
/// what it reports: the one an unnamed call resolves to, and the one it
/// is identified by. The fixture comes up under `beta` while the
/// registry's default is `alpha`, so the two refusals land on different
/// libraries and neither can stand in for the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmount_refuses_the_default_and_the_primary() -> Result<()> {
    let (sandbox, alpha, beta) = world();
    let runtime_root = tempfile::tempdir()?;
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let mut opts = RuntimeOpts::headless(None, Some("beta".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    assert_eq!(
        runtime.registry.get(None)?.name(),
        "alpha",
        "the default has to differ from the primary for this test to discriminate",
    );

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.unmount","params":{"name":"alpha"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let message = resp["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("default"),
            "the default's refusal has to say it is the default: {resp}"
        );
        let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
        assert!(
            hint.contains("libraries default"),
            "the default's refusal points at moving the pointer: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.unmount","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let message = resp["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("came up under"),
            "the primary's refusal has to say it is the bring-up selection: {resp}"
        );
        let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
        assert!(
            hint.contains("bookrack quit"),
            "the primary's refusal points at restarting the daemon: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":3,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let mut names = library_names(&resp)?;
        names.sort_unstable();
        assert_eq!(names, ["alpha", "beta"], "{resp}");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// Forking used to leave the operator with a library the daemon could
/// not see until it was restarted; the clone is served on the same
/// call now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forked_library_is_served_without_a_restart() -> Result<()> {
    let (_sandbox, _alpha, _beta) = world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(runtime_root.path()).await?;
    let clone_parent = tempfile::tempdir()?;
    let clone_root = clone_parent.path().join("clone-root");

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "library.fork",
            "params": {
                "new_name": "clone",
                "data_dir": clone_root,
                "yes": true,
            },
        });
        send(&mut w, &request.to_string()).await?;
        let resp = recv(&mut reader).await?;
        assert!(resp["error"].is_null(), "fork failed: {resp}");
        let report = resp;

        // No restart between the fork and this listing.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let mut names = library_names(&resp)?;
        names.sort_unstable();
        assert_eq!(
            names,
            ["alpha", "clone"],
            "the clone is not served without a restart: {resp}"
        );
        assert_eq!(
            report["result"]["mounted"],
            Value::Bool(true),
            "the fork report has to say whether the clone is being served: {report}"
        );

        // Served, not merely listed: a read addressed to the clone
        // reaches the clone's own root.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":3,"method":"library.info","params":{"name":"clone"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let root = resp["result"]["data_dir"]
            .as_str()
            .ok_or_else(|| eyre!("library.info carries no data_dir: {resp}"))?;
        assert!(
            root.ends_with("clone-root"),
            "the clone's read routed elsewhere: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// A clone the daemon cannot serve is still a clone: the fork reports
/// success and says it is not being served, because rolling the fork
/// back would delete a freshly built library to report a serving
/// problem.
///
/// The fixture reaches that state the way a real machine can: the
/// registry is a shared file, so another process removing an entry
/// while the daemon serves it leaves a library mounted under a name
/// the registry no longer carries. A fork may then claim that name —
/// `fork` checks the registry, which does not have it — and the mount
/// that follows collides with the library already answering to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_reports_when_the_new_library_could_not_be_mounted() -> Result<()> {
    let (sandbox, alpha, beta) = world();
    let runtime_root = tempfile::tempdir()?;
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let runtime = start(runtime_root.path()).await?;
    // Somebody else edits the registry: `beta` is served but no longer
    // registered, so its name is free as far as `fork` can tell.
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);

    let clone_parent = tempfile::tempdir()?;
    let clone_root = clone_parent.path().join("beta-clone-root");
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "library.fork",
            "params": {
                "new_name": "beta",
                "data_dir": clone_root,
                "yes": true,
            },
        });
        send(&mut w, &request.to_string()).await?;
        let resp = recv(&mut reader).await?;
        assert!(
            resp["error"].is_null(),
            "a fork that built its library must not be reported as failed: {resp}"
        );
        assert_eq!(
            resp["result"]["mounted"],
            Value::Bool(false),
            "the unserved clone was reported as served: {resp}"
        );
        assert!(
            resp["result"]["mount_error"].is_string(),
            "an unserved clone has to say why: {resp}"
        );
        // The clone is on disk and registered, so the report still
        // names where it went.
        assert!(
            resp["result"]["data_dir"]
                .as_str()
                .unwrap_or_default()
                .ends_with("beta-clone-root"),
            "{resp}"
        );

        // The served set is untouched: `beta` still names the library
        // that was already mounted, not the clone.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.info","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let root = resp["result"]["data_dir"]
            .as_str()
            .ok_or_else(|| eyre!("library.info carries no data_dir: {resp}"))?;
        assert!(
            root.ends_with("beta-root"),
            "the failed mount displaced the library already serving that name: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// Moving the default pointer at a library the daemon is not serving
/// used to be refused, which meant the pointer could only ever move
/// between whatever happened to be mounted. It mounts the target
/// instead, so the daemon serves what it is about to route unnamed
/// calls to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_default_mounts_a_registered_but_unmounted_library() -> Result<()> {
    let (sandbox, alpha, beta) = world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start(runtime_root.path()).await?;
    register_beta(sandbox, &alpha, &beta);
    assert!(
        runtime.registry.get(Some("beta")).is_err(),
        "the fixture has to start with beta registered but unmounted",
    );

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.set_default","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert!(
            resp["error"].is_null(),
            "a registered library the daemon had not mounted was refused: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let mut names = library_names(&resp)?;
        names.sort_unstable();
        assert_eq!(
            names,
            ["alpha", "beta"],
            "the new default has to be served, not just pointed at: {resp}"
        );

        // An unnamed read resolves to it, which is the whole meaning of
        // the pointer having moved.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":3,"method":"library.info"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let root = resp["result"]["data_dir"]
            .as_str()
            .ok_or_else(|| eyre!("library.info carries no data_dir: {resp}"))?;
        assert!(
            root.ends_with("beta-root"),
            "an unnamed call still resolves to the old default: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

fn library_names(resp: &Value) -> Result<Vec<&str>> {
    let entries = resp["result"]
        .as_array()
        .ok_or_else(|| eyre!("library.list did not return an array: {resp}"))?;
    Ok(entries.iter().filter_map(|e| e["name"].as_str()).collect())
}
