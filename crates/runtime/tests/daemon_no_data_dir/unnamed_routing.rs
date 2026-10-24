// SPDX-License-Identifier: Apache-2.0

//! Where a call that names no library goes, decided at the registry
//! from what is mounted and what the on-disk registry declares.
//!
//! The registry default is the route only while it is mounted; with
//! no default declared, the only mounted library is; every other
//! shape — two libraries and no default, a default the daemon does
//! not serve — is refused with `-32010` on the control socket, the
//! mounted names in the detail and `--library` in the hint. A default
//! naming no registered library counts as declared, so it refuses
//! rather than falling back to the library the daemon came up under.
//!
//! The registry env is pinned to a per-binary tempdir, so the test
//! never touches the user's real registry file. The embedder probe
//! daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

use bookrack_ops::registry::RegistryError;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::{ContextCompat, Result};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, connect, join_with_deadline, recv, send};

const INVALID_LIBRARY: i64 = -32010;

async fn call(
    reader: &mut Reader,
    w: &mut Writer,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    send(w, &req.to_string()).await?;
    recv(reader).await
}

/// The refusal an unroutable unnamed call answers with: the library
/// code, and a hint that names the flag.
fn assert_unroutable(method: &str, resp: &Value) {
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(INVALID_LIBRARY),
        "{method} without a library must be refused as unroutable: {resp}"
    );
    let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("--library"),
        "{method} must name the flag that routes the call: {resp}"
    );
}

/// Two registered libraries and no `default`: the daemon serves both
/// and routes an unnamed call to neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_libraries_without_a_registry_default_refuse_an_unnamed_call() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        None,
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let runtime_root = tempfile::tempdir()?;

    let mut opts = RuntimeOpts::headless(None, Some("beta".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    assert_eq!(
        runtime.registry.len(),
        2,
        "both registered libraries are served"
    );
    assert!(
        matches!(
            runtime.registry.get(None),
            Err(RegistryError::UnnamedAmbiguous { .. })
        ),
        "with no default declared, two libraries leave an unnamed call nowhere to go"
    );
    assert_eq!(runtime.registry.default_name()?, None);

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let resp = call(&mut reader, &mut w, 1, "library.info", Value::Null).await?;
        assert_unroutable("library.info", &resp);

        // No served row is the route, and the card still lists both.
        let status = call(&mut reader, &mut w, 2, "daemon.status", Value::Null).await?;
        let served = status["result"]["served"]
            .as_array()
            .with_context(|| format!("status carries no served rows: {status}"))?;
        assert_eq!(served.len(), 2, "{status}");
        assert!(
            served
                .iter()
                .all(|row| row["default"] == Value::Bool(false)),
            "no row may claim the unnamed route: {status}"
        );

        // A named call still resolves.
        let resp = call(
            &mut reader,
            &mut w,
            3,
            "library.info",
            json!({ "name": "alpha" }),
        )
        .await?;
        assert_eq!(resp["result"]["library_name"], "alpha", "{resp}");

        let _ = call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}

/// A `default` naming no registered library is still a declared
/// default: unnamed reads and submits are refused, and no job is
/// written in memory or on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dangling_default_refuses_unnamed_reads_and_submits() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(Some("ghost"), &[("beta", beta.as_path())]);
    let runtime_root = tempfile::tempdir()?;
    let fixture_dir = tempfile::tempdir()?;
    let book = fixture_dir.path().join("synthetic-ledger.txt");
    std::fs::write(&book, "Chapter One\n\nA synthetic ledger.\n")?;

    let mut opts = RuntimeOpts::headless(None, Some("beta".to_string()));
    opts.no_mcp = true;
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    assert_eq!(
        runtime.registry.default_name()?.as_deref(),
        Some("ghost"),
        "the pointer is carried as declared, not inferred from the entries"
    );
    let queue_state = std::sync::Arc::clone(&runtime.queue_state);
    let queue_state_path = runtime.queue_state_path.clone();

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let resp = call(&mut reader, &mut w, 1, "library.info", Value::Null).await?;
        assert_unroutable("library.info", &resp);

        let resp = call(
            &mut reader,
            &mut w,
            2,
            "ingest.submit",
            json!({ "paths": [book] }),
        )
        .await?;
        assert_unroutable("ingest.submit", &resp);

        let in_memory = queue_state
            .lock()
            .map_err(|_| eyre::eyre!("queue state lock poisoned"))?
            .jobs
            .len();
        assert_eq!(in_memory, 0, "a refused submit writes no job");
        // The worker persists an empty document at start; what must not
        // be there is a row.
        if let Ok(text) = std::fs::read_to_string(&queue_state_path) {
            let on_disk: Value = serde_json::from_str(&text)?;
            assert_eq!(
                on_disk["jobs"].as_array().map(Vec::len),
                Some(0),
                "a refused submit writes no job to the queue file: {text}"
            );
        }

        // The library the daemon came up under is reachable by name.
        let resp = call(
            &mut reader,
            &mut w,
            3,
            "library.info",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(resp["result"]["library_name"], "beta", "{resp}");

        let _ = call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
