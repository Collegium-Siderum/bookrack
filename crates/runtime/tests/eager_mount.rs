// SPDX-License-Identifier: Apache-2.0

//! Eager multi-mount bring-up: a daemon whose primary root is selected
//! through the registry mounts every registered library, routes
//! registry lookups to each of them, and reports the full set through
//! the control-plane `library.list` method.
//!
//! Ignored by default because [`bookrack_query::Library::open`] probes
//! the configured Ollama daemon for the embedding model's dimension —
//! once per mounted library here.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Result, eyre};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::UnixStream;

struct Env {
    _state: tempfile::TempDir,
    _roots: tempfile::TempDir,
}

static ENV: OnceLock<Env> = OnceLock::new();

/// Pin the daemon state directory and a two-library registry into
/// per-binary tempdirs: `alpha` (the registry default) and `beta`,
/// each with its own data root.
fn isolate_env() {
    ENV.get_or_init(|| {
        let state = tempfile::tempdir().expect("daemon state tempdir");
        let roots = tempfile::tempdir().expect("library roots tempdir");
        let alpha = roots.path().join("alpha-root");
        let beta = roots.path().join("beta-root");
        std::fs::create_dir_all(&alpha).expect("alpha root");
        std::fs::create_dir_all(&beta).expect("beta root");
        let registry = roots.path().join("registry.toml");
        std::fs::write(
            &registry,
            format!(
                "default = \"alpha\"\n\n\
                 [libraries.alpha]\ndata_dir = {alpha:?}\n\n\
                 [libraries.beta]\ndata_dir = {beta:?}\n",
                alpha = alpha.display().to_string(),
                beta = beta.display().to_string(),
            ),
        )
        .expect("write registry");
        // SAFETY: env is mutated exactly once, inside
        // `OnceLock::get_or_init`'s single-initialization guarantee,
        // as the first statement of every test in this binary, before
        // any concurrent env reads.
        unsafe {
            std::env::set_var("BOOKRACK_DAEMON_STATE_DIR", state.path());
            std::env::set_var("BOOKRACK_REGISTRY", &registry);
            std::env::remove_var("BOOKRACK_DATA_DIR");
        }
        Env {
            _state: state,
            _roots: roots,
        }
    });
}

async fn send(stream: &mut WriteHalf<UnixStream>, line: &str) -> Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn recv(reader: &mut Lines<BufReader<ReadHalf<UnixStream>>>) -> Result<Value> {
    let line = reader
        .next_line()
        .await?
        .ok_or_else(|| eyre!("connection closed before response"))?;
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn registry_selection_mounts_every_registered_library() -> Result<()> {
    isolate_env();
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
    let shutdown_tx = runtime.shutdown_tx.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = UnixStream::connect(&sock).await?;
        let (r, mut w) = tokio::io::split(stream);
        let mut reader = BufReader::new(r).lines();
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
    let _ = shutdown_tx;
    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    Ok(())
}

/// Both served roots hold the daemon's root lock while it runs — the
/// eager-mount counterpart of the single-library lock test in
/// `daemon_lifecycle.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn every_served_root_is_locked_while_the_daemon_runs() -> Result<()> {
    use bookrack_session::{RootLock, is_root_lock_conflict};

    isolate_env();
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
