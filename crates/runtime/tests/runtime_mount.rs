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
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
    (sandbox, alpha, beta)
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

fn library_names(resp: &Value) -> Result<Vec<&str>> {
    let entries = resp["result"]
        .as_array()
        .ok_or_else(|| eyre!("library.list did not return an array: {resp}"))?;
    Ok(entries.iter().filter_map(|e| e["name"].as_str()).collect())
}
