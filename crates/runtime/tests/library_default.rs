// SPDX-License-Identifier: Apache-2.0

//! `library.set_default` success path: the handler persists the new
//! default to the on-disk registry, refreshes the daemon's in-memory
//! pointer, and publishes a `library.changed` event.
//!
//! The registry env is pinned to a per-binary tempdir, so the test
//! never touches the user's real registry file. The embedder probe
//! daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{Context, Result};
use serde_json::Value;

use crate::common::{await_channel, connect, join_with_deadline, recv, send};

/// Isolate the process and seed a two-library registry: `alpha` (the
/// registry default) and `beta`, each with its own data root under the
/// sandbox.
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
async fn set_default_flips_the_registry_and_the_in_memory_pointer() -> Result<()> {
    let sandbox = world();
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

    let registry_path = sandbox.registry_path();
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
