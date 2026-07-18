// SPDX-License-Identifier: Apache-2.0

//! Phase 5 second-launch contract: a second `bookrack run` against a
//! runtime directory whose lock is held prints the recorded address
//! and exits zero; a lock pointing at a dead daemon exits with status
//! three so the operator removes it by hand.
//!
//! Ignored by default because the spawned `bookrack` binary opens an
//! embedding client; without a reachable Ollama daemon the second
//! launch never finishes its `daemon.version` probe.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::Result;

static DAEMON_STATE_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Redirect the daemon state directory into a per-binary tempdir so
/// bring-up (in-process and in spawned `bookrack` subprocesses, which
/// inherit the environment) never touches the user's real per-user
/// data directory.
fn isolate_daemon_state_dir() {
    DAEMON_STATE_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("daemon state tempdir");
        // SAFETY: env is mutated exactly once, inside
        // `OnceLock::get_or_init`'s single-initialization guarantee,
        // as the first statement of every test in this binary, before
        // any concurrent env reads.
        unsafe { std::env::set_var("BOOKRACK_DAEMON_STATE_DIR", dir.path()) };
        dir
    });
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn cli_second_launch_prints_addr_and_exits_zero() -> Result<()> {
    isolate_daemon_state_dir();
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
    ))
    .await?;
    let pid = std::process::id();
    let sock_path = runtime.control_sock.path.clone();
    let shutdown_tx = runtime.shutdown_tx.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    // The spawned binary resolves its library from the environment, so
    // both directories are pinned explicitly; otherwise the test would
    // depend on whatever library the host machine has configured.
    let runtime_dir_for_subprocess = runtime_root.path().to_path_buf();
    let data_dir_for_subprocess = data_root.path().to_path_buf();
    let subprocess = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bookrack"))
            .args(["run", "--no-mcp"])
            .env("BOOKRACK_RUNTIME_DIR", runtime_dir_for_subprocess)
            .env("BOOKRACK_DATA_DIR", data_dir_for_subprocess)
            .output()
            .await
    });

    let out = subprocess.await??;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}");
    assert!(stdout.contains("control_sock="), "stdout: {stdout}");
    assert!(stdout.contains(&format!("pid={pid}")), "stdout: {stdout}");
    assert!(
        stdout.contains(&sock_path.display().to_string()),
        "stdout: {stdout}"
    );

    let _ = shutdown_tx.send(());
    runtime.run_until_shutdown(None, repl_handle).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn status_against_a_live_daemon_prints_the_card() -> Result<()> {
    isolate_daemon_state_dir();
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
    ))
    .await?;
    let shutdown_tx = runtime.shutdown_tx.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let runtime_dir_for_subprocess = runtime_root.path().to_path_buf();
    let data_dir_for_subprocess = data_root.path().to_path_buf();
    let subprocess = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bookrack"))
            .args(["status"])
            .env("BOOKRACK_RUNTIME_DIR", runtime_dir_for_subprocess)
            .env("BOOKRACK_DATA_DIR", data_dir_for_subprocess)
            .output()
            .await
    });

    let out = subprocess.await??;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}");
    for needle in [
        "daemon.state",
        "library.name",
        "library.data_dir",
        "queue.pending",
    ] {
        assert!(
            stdout.contains(needle),
            "missing {needle} in card: {stdout}"
        );
    }

    let _ = shutdown_tx.send(());
    runtime.run_until_shutdown(None, repl_handle).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_lock_exits_three() -> Result<()> {
    isolate_daemon_state_dir();
    use std::io::Write;

    use fs2::FileExt;

    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let lock_path = runtime_root.path().join("bookrack.tty.lock");
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    holder.try_lock_exclusive()?;
    let mut writer = &holder;
    writer.set_len(0)?;
    writeln!(writer, "pid=999999")?;
    writeln!(writer, "mcp=disabled")?;
    writeln!(
        writer,
        "control_sock={}",
        runtime_root
            .path()
            .join("bookrack-no-such-sock-phase5.sock")
            .display()
    )?;
    writer.flush()?;

    // A pinned data dir keeps the run from depending on the host's
    // configured library; the stale-lock check must be what terminates
    // the process, not library resolution.
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_bookrack"))
        .args(["run", "--no-mcp"])
        .env("BOOKRACK_RUNTIME_DIR", runtime_root.path())
        .env("BOOKRACK_DATA_DIR", data_root.path())
        .output()
        .await?;
    assert_eq!(
        out.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(holder);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn daemon_shutdown_rpc_exits_cleanly() -> Result<()> {
    isolate_daemon_state_dir();
    // Covers one representative leg of the five shutdown paths that
    // share `shutdown_tx.send(())`: the control-plane `daemon.shutdown`
    // RPC. The other four (SIGINT, REPL disconnect, MCP
    // `session.shutdown`, GUI tray) all route through the same
    // broadcast and are covered by their respective component tests.
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
    ))
    .await?;
    let sock_path = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let socket = bookrack_control_client::ControlSocket::from_path(sock_path);
        let client = bookrack_control_client::connect(&socket).await?;
        client.shutdown().await?;
        eyre::Ok(())
    });

    runtime.run_until_shutdown(None, repl_handle).await?;
    driver.await??;
    Ok(())
}
