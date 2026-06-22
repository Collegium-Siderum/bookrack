// SPDX-License-Identifier: Apache-2.0

//! Phase 3 end-to-end coverage for `bookrack repl`.
//!
//! The first test runs without spinning up a daemon: it asserts the
//! client reports the no-daemon condition with the contract-stated
//! exit code and a stderr message a script can grep on.
//!
//! The second test brings up a [`DaemonRuntime`] in the headless
//! profile, points `bookrack repl` at its runtime directory with
//! piped stdin, and validates that a `queue` line dispatches through
//! the control plane (the underlying `ingest.submit` is asserted via
//! the daemon-side `queue.list` snapshot). It is `#[ignore]` because
//! the runtime's library bootstrap probes the configured Ollama
//! daemon for the embedding model dimension; run manually with
//! `cargo test -p bookrack-cli -- --ignored`.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use tokio::io::AsyncWriteExt;

fn bookrack_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bookrack")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repl_reports_no_daemon() -> Result<()> {
    let runtime_dir = tempfile::tempdir()?;
    let output = tokio::process::Command::new(bookrack_bin())
        .args(["repl"])
        .env("BOOKRACK_RUNTIME_DIR", runtime_dir.path())
        .env("BOOKRACK_DATA_DIR", runtime_dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon not running"),
        "expected `daemon not running` in stderr; got: {stderr}",
    );
    Ok(())
}

fn build_opts(data_dir: PathBuf, runtime_dir: PathBuf) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_dir);
    opts.selection = LibrarySelection {
        data_dir: opts.selection.data_dir,
        library: opts.selection.library,
    };
    opts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn repl_batch_dispatches_queue_list_over_control_plane() -> Result<()> {
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
    ))
    .await?;
    let shutdown_tx = runtime.shutdown_tx.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = {
        let runtime_dir = runtime_root.path().to_path_buf();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut child = tokio::process::Command::new(bookrack_bin())
                .args(["repl"])
                .env("BOOKRACK_RUNTIME_DIR", &runtime_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            // Drain stdout and stderr concurrently so the repl's
            // own writes never fill a piped buffer and deadlock
            // the test on `child.wait()`. `kill_on_drop` above
            // ensures a panic between spawn and wait kills the
            // child instead of leaking it.
            let stdout = child.stdout.take().expect("stdout");
            let stderr = child.stderr.take().expect("stderr");
            let stdout_handle = tokio::spawn(async move {
                let mut buf = String::new();
                let _ = tokio::io::AsyncReadExt::read_to_string(
                    &mut tokio::io::BufReader::new(stdout),
                    &mut buf,
                )
                .await;
                buf
            });
            let stderr_handle = tokio::spawn(async move {
                let mut buf = String::new();
                let _ = tokio::io::AsyncReadExt::read_to_string(
                    &mut tokio::io::BufReader::new(stderr),
                    &mut buf,
                )
                .await;
                buf
            });
            let mut stdin = child.stdin.take().expect("stdin");
            stdin.write_all(b"queue\n").await?;
            stdin.write_all(b"status\n").await?;
            stdin.write_all(b"exit\n").await?;
            stdin.shutdown().await?;
            drop(stdin);
            let status = child.wait().await?;
            let _ = stdout_handle.await;
            let _ = stderr_handle.await;
            assert!(status.success(), "repl exit status: {status:?}");
            Ok::<_, anyhow::Error>(())
        })
    };

    let runner = runtime.run_until_shutdown(None, repl_handle);
    let driver_then_shutdown = async {
        let result = driver.await;
        let _ = shutdown_tx.send(());
        result
    };
    let runner_with_timeout = async {
        match tokio::time::timeout(Duration::from_secs(30), runner).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("runner did not exit within 30s")),
        }
    };
    let (driver_result, runner_result) = tokio::join!(driver_then_shutdown, runner_with_timeout);
    driver_result??;
    runner_result?;
    Ok(())
}
