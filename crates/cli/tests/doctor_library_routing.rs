// SPDX-License-Identifier: Apache-2.0

//! `bookrack --library <name> doctor` and `diagnose` reach the named
//! library through a running daemon, along the whole path an
//! operator's invocation takes.
//!
//! The selection has three hops to survive: the flag parsed into the
//! process-wide pending selection, that selection settled into a name
//! once a connection exists, and the name injected into the call. The
//! second hop is the one `doctor` has its own connection code for, so
//! it is the hop an RPC-level test cannot see dropped.
//!
//! The daemon comes up in this process under `alpha` with `beta` also
//! registered; `process_env` isolates this binary's view of the host and
//! the same sandbox is handed to the child.

#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, bookrack_cmd, process_env};
use eyre::{Result, eyre};
use serde_json::Value;

fn seed_root(root: &Path) -> Result<()> {
    for db in ["catalog.db", "papers_catalog.db"] {
        bookrack_catalog::Catalog::open(&root.join(db))?;
    }
    for db in ["corpus.db", "papers_corpus.db"] {
        bookrack_corpus::Corpus::open(&root.join(db))?;
    }
    Ok(())
}

/// The `data root` row of a `doctor --json` report.
fn data_root_value(report: &Value) -> Result<String> {
    report["rows"]
        .as_array()
        .ok_or_else(|| eyre!("no rows in the report: {report}"))?
        .iter()
        .find(|row| row["label"] == "data root")
        .and_then(|row| row["value"].as_str())
        .map(str::to_string)
        .ok_or_else(|| eyre!("no data root row in the report: {report}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_named_library_reaches_doctor_and_diagnose_through_the_daemon() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    seed_root(&alpha)?;
    seed_root(&beta)?;
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let runtime_root = tempfile::tempdir()?;

    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    let shutdown_tx = runtime.shutdown_tx.clone();

    let runtime_dir_for_child = runtime_root.path().to_path_buf();
    let child = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut outputs = Vec::new();
        for args in [
            &["--json", "--library", "beta", "doctor"][..],
            &["--json", "--library", "beta", "diagnose"][..],
        ] {
            let out = tokio::process::Command::from(
                bookrack_cmd!(sandbox)
                    .runtime_dir(&runtime_dir_for_child)
                    .without_data_dir()
                    .build(),
            )
            .args(args)
            .output()
            .await?;
            outputs.push(out);
        }
        Ok::<_, std::io::Error>(outputs)
    });
    let outputs = child.await??;

    let _ = shutdown_tx.send(());
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    runtime.run_until_shutdown(None, repl_handle).await?;

    let beta_str = beta.display().to_string();

    let doctor = &outputs[0];
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    let stderr = String::from_utf8_lossy(&doctor.stderr);
    let report: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("doctor --json is not JSON ({e}): {stdout}\n{stderr}"));
    assert_eq!(
        data_root_value(&report)?,
        beta_str,
        "doctor must report the library the flag named, not the one the daemon \
         was brought up under; stderr={stderr}"
    );

    let diagnose = &outputs[1];
    let stdout = String::from_utf8_lossy(&diagnose.stdout);
    let stderr = String::from_utf8_lossy(&diagnose.stderr);
    let result: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("diagnose --json is not JSON ({e}): {stdout}\n{stderr}"));
    let out_path = result["out_path"]
        .as_str()
        .ok_or_else(|| eyre!("no out_path in {result}"))?;
    assert!(
        out_path.starts_with(&beta_str),
        "the bundle must land under the named library's root, got {out_path}; stderr={stderr}"
    );
    Ok(())
}
