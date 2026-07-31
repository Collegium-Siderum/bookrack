// SPDX-License-Identifier: Apache-2.0

//! Eager multi-mount bring-up: a daemon whose primary root is selected
//! through the registry mounts every registered library, routes
//! registry lookups to each of them, and reports the full set through
//! the control-plane `library.list` method.
//!
//! The embedder probe daemon bring-up performs — once per mounted
//! library here — is answered by `bookrack_test_support::EmbedStub`,
//! so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{Result, eyre};

use crate::common::{connect, join_with_deadline, recv, send};

/// Isolate the process and seed a two-library registry: `alpha` (the
/// registry default) and `beta`, each with its own data root under the
/// sandbox. Seeding is guarded so a second test in this binary reuses
/// the file rather than rewriting it.
fn world() -> &'static Sandbox {
    static SEEDED: OnceLock<()> = OnceLock::new();
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    SEEDED.get_or_init(|| {
        let alpha = sandbox.data_root("alpha-root");
        let beta = sandbox.data_root("beta-root");
        sandbox.write_registry_entries(
            Some("alpha"),
            &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
        );
    });
    sandbox
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registry_selection_mounts_every_registered_library() -> Result<()> {
    world();
    let runtime_root = tempfile::tempdir()?;

    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());

    let runtime = DaemonRuntime::start(opts).await?;

    // Both registered libraries answer a registry lookup; the default
    // pointer starts at the registry's default entry.
    assert!(runtime.registry.get(Some("alpha")).is_ok());
    assert!(runtime.registry.get(Some("beta")).is_ok());
    assert_eq!(runtime.registry.get(None)?.name(), "alpha");

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let entries = resp["result"]
            .as_array()
            .ok_or_else(|| eyre!("library.list did not return an array: {resp}"))?;
        let mut names: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["alpha", "beta"], "{resp}");
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// Both served roots hold the daemon's root lock while it runs — the
/// eager-mount counterpart of the single-library lock test in
/// `daemon_lifecycle.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_served_root_is_locked_while_the_daemon_runs() -> Result<()> {
    use bookrack_session::{RootLock, is_root_lock_conflict};

    world();
    let runtime_root = tempfile::tempdir()?;
    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;

    let alpha_root = PathBuf::from(runtime.cfg.data_dir());
    let beta_root = alpha_root
        .parent()
        .expect("shared parent")
        .join("beta-root");
    for root in [&alpha_root, &beta_root] {
        let err = match RootLock::acquire(root, std::process::id(), "test") {
            Ok(_) => panic!(
                "{} must be locked while the daemon serves it",
                root.display()
            ),
            Err(e) => e,
        };
        assert!(is_root_lock_conflict(&err), "{err}");
    }

    let shutdown_tx = runtime.shutdown_tx.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let _ = shutdown_tx.send(());
    runtime.run_until_shutdown(None, repl_handle).await?;
    Ok(())
}
