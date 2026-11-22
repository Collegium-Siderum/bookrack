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

/// `daemon.status` and the daemon-gathered `doctor.gather` count the
/// descriptors of the process answering them. The daemon here runs in
/// the test's own process, so files the test opens are the daemon's
/// descriptors too: a count read off anything but the live process
/// would not move when they open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_and_doctor_count_the_daemons_open_descriptors() -> Result<()> {
    const OPENED: u64 = 32;
    // The daemon's own tasks may open or close a few descriptors
    // between the two reads; the window absorbs those.
    const SLACK: u64 = 8;
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let scratch = tempfile::tempdir()?;
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
        send(&mut writer, r#"{"jsonrpc":"2.0","id":1,"method":"status"}"#).await?;
        let before = recv(&mut reader).await?;
        let fds = &before["result"]["fds"];
        assert_eq!(fds["pressure"], "normal", "{before}");
        assert!(
            fds["soft_limit"].is_u64() || fds["soft_limit"].is_null(),
            "{before}"
        );
        let open_before = fds["open"]
            .as_u64()
            .unwrap_or_else(|| panic!("status carries no open count: {before}"));

        let files: Vec<std::fs::File> = (0..OPENED)
            .map(|i| std::fs::File::create(scratch.path().join(format!("f{i}"))))
            .collect::<std::io::Result<_>>()?;
        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":2,"method":"daemon.status"}"#,
        )
        .await?;
        let during = recv(&mut reader).await?;
        let open_during = during["result"]["fds"]["open"].as_u64().unwrap_or(0);
        assert!(
            open_during.abs_diff(open_before + OPENED) <= SLACK,
            "opening {OPENED} files moved the daemon's count from {open_before} to {open_during}"
        );

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":3,"method":"doctor.gather"}"#,
        )
        .await?;
        let report = recv(&mut reader).await?;
        drop(files);
        let row = report["result"]["rows"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["label"] == "fd limit"))
            .unwrap_or_else(|| panic!("no fd limit row: {report}"))
            .clone();
        let value = row["value"].as_str().unwrap_or_default();
        let open = value
            .split_once(' ')
            .filter(|(_, rest)| rest.contains("open"))
            .and_then(|(n, _)| n.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("the fd limit row carries no open count: {row}"));
        assert!(
            open.abs_diff(open_during) <= SLACK,
            "doctor counted {open}, status {open_during}: {row}"
        );

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":4,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
