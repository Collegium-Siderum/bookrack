// SPDX-License-Identifier: Apache-2.0

//! Bring-up on a path-class root the registry does not know.
//!
//! A root carrying an identity manifest is registered under the
//! manifest's name and the daemon serves the registry, as it would
//! had the operator run `libraries add` first; the `default` pointer
//! is left as it was. A root without a manifest is served alone and
//! the registry is not touched.

#![cfg(unix)]

mod common;

use bookrack_config::{LibraryKind, new_manifest, write_manifest};
use bookrack_runtime::DaemonRuntime;
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::Result;
use serde_json::{Value, json};

use crate::common::{Reader, Writer, connect, join_with_deadline, recv, send};

async fn call(writer: &mut Writer, reader: &mut Reader, id: u64, method: &str) -> Result<Value> {
    let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": null});
    send(writer, &frame.to_string()).await?;
    recv(reader).await
}

/// The `default = "..."` line of a registry file, if any.
fn default_line(registry: &str) -> Option<String> {
    registry
        .lines()
        .find(|line| line.starts_with("default = "))
        .map(str::to_string)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manifest_bearing_root_is_registered_and_the_registry_is_served() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let runtime_root = tempfile::tempdir()?;
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let gamma = sandbox.data_root("gamma-root");
    write_manifest(&gamma, &new_manifest("gamma", LibraryKind::Test, None))?;
    let before = std::fs::read_to_string(sandbox.registry_path())?;

    let runtime = DaemonRuntime::start(common::build_opts(
        gamma.clone(),
        runtime_root.path().to_path_buf(),
        false,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let registry_path = sandbox.registry_path();
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let status = call(&mut w, &mut reader, 1, "daemon.status").await?;
        let result = &status["result"];

        let written = std::fs::read_to_string(&registry_path)?;
        assert!(
            written.contains("[libraries.gamma]"),
            "the selected root must be registered under its manifest name: {written}",
        );
        assert_eq!(
            default_line(&written),
            default_line(&before),
            "registering must not move the default pointer: {written}",
        );
        assert_eq!(
            result["library"],
            json!("gamma"),
            "the daemon must serve the root under its new name: {result}"
        );
        let served = result["served"]
            .as_array()
            .unwrap_or_else(|| panic!("served must be an array: {result}"));
        assert_eq!(served.len(), 3, "the registry is served in full: {result}");
        assert_eq!(
            result["auto_registered"],
            json!(["gamma"]),
            "the status must report what bring-up registered: {result}"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_without_a_manifest_is_served_alone_and_not_registered() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let runtime_root = tempfile::tempdir()?;
    let alpha = sandbox.data_root("alpha-root");
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
    let delta = sandbox.data_root("delta-root");
    let before = std::fs::read_to_string(sandbox.registry_path())?;

    let runtime = DaemonRuntime::start(common::build_opts(
        delta.clone(),
        runtime_root.path().to_path_buf(),
        false,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let registry_path = sandbox.registry_path();
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let status = call(&mut w, &mut reader, 1, "daemon.status").await?;
        let result = &status["result"];

        let after = std::fs::read_to_string(&registry_path)?;
        assert_eq!(after, before, "a manifestless root must not be registered");
        assert_eq!(result["library"], Value::Null, "{result}");
        let served = result["served"]
            .as_array()
            .unwrap_or_else(|| panic!("served must be an array: {result}"));
        assert_eq!(served.len(), 1, "the root is served alone: {result}");
        assert_eq!(result["auto_registered"], json!([]), "{result}");

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
