// SPDX-License-Identifier: Apache-2.0

//! End-to-end Phase 1 control-plane sanity check.
//!
//! Boots a [`DaemonRuntime`] in the headless profile against a fresh
//! tempdir, connects to the bound control socket, and drives the full
//! handshake the contract guarantees:
//!
//! 1. `daemon.version` returns the workspace version.
//! 2. `events.subscribe` returns `{ subscribed: true }` and then the
//!    snapshot bundle, one notification per contract channel, in
//!    contract order.
//! 3. `doctor.gather` returns a structured report.
//! 4. `daemon.shutdown` triggers a `daemon.state = stopping` notification.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use eyre::Result;
use serde_json::Value;

use crate::common::{build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

/// The documented `events.subscribe` snapshot bundle, in broadcast
/// order. Pinned literally so a channel added to or removed from the
/// runtime's `SNAPSHOT_CHANNELS` fails here until the control-plane
/// contract moves with it.
const SNAPSHOT_CONTRACT: [&str; 7] = [
    "daemon.state",
    "queue.list",
    "queue.tick",
    "library.list",
    "library.changed",
    "mcp.availability",
    "daemon.version",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_loop_subscribe_doctor_shutdown() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        false,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut writer) = connect(&sock).await?;

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":1,"method":"daemon.version"}"#,
        )
        .await?;
        let v = recv(&mut reader).await?;
        assert!(v["result"]["version"].as_str().is_some(), "{v}");

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":2,"method":"events.subscribe"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["result"]["subscribed"], Value::from(true), "{resp}");

        let mut channels = Vec::new();
        for _ in 0..SNAPSHOT_CONTRACT.len() {
            let notif = recv(&mut reader).await?;
            channels.push(
                notif["params"]["channel"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        assert_eq!(channels, SNAPSHOT_CONTRACT, "snapshot bundle drifted");

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":3,"method":"doctor.gather"}"#,
        )
        .await?;
        let v = recv(&mut reader).await?;
        assert!(v["result"]["rows"].is_array(), "{v}");

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":4,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;

        let stopping = recv(&mut reader).await?;
        assert_eq!(stopping["params"]["channel"], "daemon.state");
        assert_eq!(stopping["params"]["value"], "stopping");
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
