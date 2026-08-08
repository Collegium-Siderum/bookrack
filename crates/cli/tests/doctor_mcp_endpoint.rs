// SPDX-License-Identifier: Apache-2.0

//! `bookrack doctor` reports on the MCP endpoint, through the whole
//! path an operator's invocation takes.
//!
//! This is the surface the endpoint was missing from: doctor checked
//! the data root, PDFium, the databases, the registry, the embed
//! backend and the reranker, and never the one address agent clients
//! connect to. The row is asserted from a child `bookrack doctor`
//! against a live daemon — control plane, `doctor.gather`, the probe,
//! the rendered report — because every intermediate layer is where the
//! address could be swapped for the configured one.
//!
//! The daemon comes up in this process, with a real MCP listener on a
//! kernel-assigned port; `process_env` isolates this binary's view of
//! the host and the same sandbox is handed to the child.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, bookrack_cmd, process_env};
use eyre::Result;
use serde_json::Value;

fn opts_serving_mcp(data_dir: PathBuf, runtime_dir: PathBuf) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = false;
    opts.mcp_addr = Some("127.0.0.1:0".parse().expect("address parses"));
    opts.runtime_dir = Some(runtime_dir);
    opts
}

/// The `MCP endpoint` row of a `doctor --json` report.
fn endpoint_row(report: &Value) -> &Value {
    report["rows"]
        .as_array()
        .expect("the report carries rows")
        .iter()
        .find(|row| row["label"] == "MCP endpoint")
        .unwrap_or_else(|| panic!("no MCP endpoint row in the report: {report}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_reports_the_endpoint_the_daemon_actually_serves() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;

    let mut runtime = DaemonRuntime::start(opts_serving_mcp(
        data_root.path().into(),
        runtime_root.path().into(),
    ))
    .await?;
    let shutdown_tx = runtime.shutdown_tx.clone();
    let served = runtime.mcp_label.clone();
    let mcp_handle = bookrack_mcp::spawn_listener(&mut runtime);
    assert!(mcp_handle.is_some(), "the listener task must be running");

    let runtime_dir_for_child = runtime_root.path().to_path_buf();
    let data_dir_for_child = data_root.path().to_path_buf();
    let child = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::process::Command::from(
            bookrack_cmd!(sandbox)
                .runtime_dir(runtime_dir_for_child)
                .data_dir(data_dir_for_child)
                .build(),
        )
        .args(["--json", "doctor"])
        .output()
        .await
    });

    let out = child.await??;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("doctor --json is not JSON ({e}): {stdout}"));

    let row = endpoint_row(&report);
    assert_eq!(
        row["value"], served,
        "the row must report the address this daemon bound, not the configured one: {row}"
    );
    assert_eq!(
        row["status"], "ok",
        "a live listener answering as bookrack is a healthy row: {row}"
    );

    let _ = shutdown_tx.send(());
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    runtime.run_until_shutdown(mcp_handle, repl_handle).await?;
    Ok(())
}
