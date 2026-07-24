// SPDX-License-Identifier: Apache-2.0

//! `library.set_default` success path: the handler persists the new
//! default to the on-disk registry, refreshes the daemon's in-memory
//! pointer, and publishes a `library.changed` event.
//!
//! The registry env is pinned to a per-binary tempdir, so the test
//! never touches the user's real registry file. The embedder probe
//! daemon bring-up performs is answered by the loopback stub in
//! `common`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Context, Result};
use serde_json::Value;

use crate::common::{await_channel, connect, embed_stub_url, join_with_deadline, recv, send};

struct Env {
    _state: tempfile::TempDir,
    _roots: tempfile::TempDir,
    registry: PathBuf,
}

static ENV: OnceLock<Env> = OnceLock::new();

/// Pin the daemon state directory and a two-library registry into
/// per-binary tempdirs: `alpha` (the registry default) and `beta`,
/// each with its own data root.
fn isolate_env() -> &'static Env {
    embed_stub_url();
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
            registry,
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_default_flips_the_registry_and_the_in_memory_pointer() -> Result<()> {
    let env = isolate_env();
    let runtime_root = tempfile::tempdir()?;

    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;

    // The pointer starts at the registry's default entry.
    let registry_handle = std::sync::Arc::clone(&runtime.registry);
    assert_eq!(registry_handle.get(None)?.name(), "alpha");

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let registry_path = env.registry.clone();
    let driver = tokio::spawn(async move {
        // Observer connection: the flip must broadcast library.changed.
        let (mut obs_reader, mut obs_w) = connect(&sock).await?;
        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#,
        )
        .await?;
        let resp = recv(&mut obs_reader).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");
        // Drain the snapshot bundle so the awaited event below is the
        // mutation broadcast, not the pre-mutation snapshot.
        for _ in 0..7 {
            let _ = recv(&mut obs_reader).await?;
        }

        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.set_default","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["result"]["ok"], Value::Bool(true), "{resp}");
        assert_eq!(resp["result"]["name"].as_str(), Some("beta"), "{resp}");

        // Disk first: the on-disk registry now carries the new default,
        // so the change survives a daemon restart.
        let registry_text =
            std::fs::read_to_string(&registry_path).context("read registry file")?;
        assert!(
            registry_text.contains(r#"default = "beta""#),
            "registry file must persist the new default: {registry_text}"
        );

        // The flip broadcasts a library.changed naming the new default.
        let event = await_channel(&mut obs_reader, "library.changed", Duration::from_secs(5))
            .await
            .context("library.changed after set_default")?;
        assert_eq!(
            event["params"]["value"]["library"].as_str(),
            Some("beta"),
            "{event}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    // Memory follows disk: the daemon's in-memory pointer — a cache of
    // the on-disk value — was refreshed by the handler.
    assert_eq!(
        registry_handle.get(None)?.name(),
        "beta",
        "the in-memory default pointer must follow the persisted flip"
    );
    Ok(())
}
