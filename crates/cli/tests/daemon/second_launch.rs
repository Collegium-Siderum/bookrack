// SPDX-License-Identifier: Apache-2.0

//! Phase 5 second-launch contract: a second `bookrack run` against a
//! runtime directory whose lock is held prints the recorded address
//! and exits zero; a held lock whose recorded daemon does not answer
//! exits with status three.
//!
//! The daemon comes up in this process and again as a spawned
//! `bookrack` subprocess, so both halves are isolated: `process_env`
//! redirects this binary's own view of the host, and the same sandbox
//! is handed to `bookrack_cmd!` for the children. The embedder probe
//! bring-up performs is answered by [`EmbedStub`], so no Ollama daemon
//! is required.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{DaemonProcess, EmbedStub, ProcessEnv, bookrack_cmd, process_env};
use eyre::Result;

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
async fn cli_second_launch_prints_addr_and_exits_zero() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon());
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
        tokio::process::Command::from(
            bookrack_cmd!(sandbox)
                .runtime_dir(runtime_dir_for_subprocess)
                .data_dir(data_dir_for_subprocess)
                .ollama_url(EmbedStub::shared().url())
                .build(),
        )
        .args(["run", "--no-mcp"])
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
async fn status_against_a_live_daemon_prints_the_card() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon());
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
        tokio::process::Command::from(
            bookrack_cmd!(sandbox)
                .runtime_dir(runtime_dir_for_subprocess)
                .data_dir(data_dir_for_subprocess)
                .ollama_url(EmbedStub::shared().url())
                .build(),
        )
        .args(["status"])
        .output()
        .await
    });

    let out = subprocess.await??;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}");
    for needle in [
        "daemon.instance_id",
        "daemon.runtime_dir",
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
    let sandbox = process_env(ProcessEnv::daemon());
    use std::io::Write;

    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let lock_path = runtime_root.path().join("bookrack.tty.lock");
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    holder.try_lock()?;
    let mut writer = &holder;
    writer.set_len(0)?;
    writeln!(writer, "pid=999999")?;
    writeln!(writer, "mcp=disabled")?;
    writeln!(
        writer,
        "control_sock={}",
        runtime_root.path().join("control.sock").display()
    )?;
    writer.flush()?;

    // A pinned data dir keeps the run from depending on the host's
    // configured library; the stale-lock check must be what terminates
    // the process, not library resolution.
    let out = tokio::process::Command::from(
        bookrack_cmd!(sandbox)
            .runtime_dir(runtime_root.path())
            .data_dir(data_root.path())
            .ollama_url(EmbedStub::shared().url())
            .build(),
    )
    .args(["run", "--no-mcp"])
    .output()
    .await?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "stderr: {stderr}");
    assert!(
        stderr.contains("is held, and the daemon it records does not answer"),
        "the summary states what the probe established: {stderr}"
    );
    assert!(
        stderr.contains("The lock records pid 999999"),
        "the detail names the recorded pid: {stderr}"
    );
    let hint = stderr
        .lines()
        .find(|line| line.trim_start().starts_with("hint:"))
        .unwrap_or_else(|| panic!("no hint line in: {stderr}"));
    assert!(hint.contains("lsof"), "{hint}");
    assert!(hint.contains("ps -p"), "{hint}");
    assert!(!stderr.contains("rm "), "no removal step: {stderr}");
    drop(holder);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_rpc_exits_cleanly() -> Result<()> {
    process_env(ProcessEnv::daemon());
    // Covers one leg of the three shutdown paths that go through
    // `initiate_shutdown`: the control-plane `daemon.shutdown` RPC. The
    // other two, a platform signal and the MCP `session.shutdown` tool,
    // are covered by their own component tests.
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

/// A held session lock whose `control_sock=` line names a live socket in
/// another directory is refused by `run`, by `status`, and by a routed
/// command, and nothing dials the socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_lock_pointing_to_foreign_socket_is_refused_without_dialing() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon());
    use std::io::Write;

    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let other_dir = tempfile::tempdir()?;
    let foreign = other_dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&foreign)?;
    listener.set_nonblocking(true)?;

    let lock_path = runtime_root.path().join("bookrack.tty.lock");
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    holder.try_lock()?;
    let mut writer = &holder;
    writeln!(writer, "pid=999999")?;
    writeln!(writer, "mcp=disabled")?;
    writeln!(writer, "control_sock={}", foreign.display())?;
    writer.flush()?;

    for (args, code) in [
        (&["run", "--no-mcp"][..], 1),
        (&["status"][..], 1),
        (&["queue", "list"][..], 2),
    ] {
        let out = tokio::process::Command::from(
            bookrack_cmd!(sandbox)
                .runtime_dir(runtime_root.path())
                .data_dir(data_root.path())
                .ollama_url(EmbedStub::shared().url())
                .build(),
        )
        .args(args)
        .output()
        .await?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "{args:?} stderr: {stderr}");
        assert!(
            stderr.contains("invalid control socket"),
            "{args:?} names the refused endpoint: {stderr}"
        );
        assert!(
            listener.accept().is_err(),
            "{args:?} dialed the foreign socket"
        );
    }
    drop(holder);
    Ok(())
}

/// A daemon given a runtime directory that is relative, or that climbs
/// through `..`, records an absolute control socket free of both, and a
/// client pointed at the same directory from elsewhere reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relative_runtime_dir_is_recorded_as_a_clean_absolute_path() -> Result<()> {
    use std::path::Component;

    let sandbox = process_env(ProcessEnv::daemon());
    for given in ["./rt", "side/../rt"] {
        let data_root = tempfile::tempdir()?;
        let work = tempfile::tempdir()?;
        std::fs::create_dir(work.path().join("side"))?;
        let runtime_dir = work.path().join("rt");
        let lock_path = runtime_dir.join("bookrack.tty.lock");

        let mut daemon_cmd = tokio::process::Command::from(
            bookrack_cmd!(sandbox)
                .data_dir(data_root.path())
                .ollama_url(EmbedStub::shared().url())
                .build(),
        );
        daemon_cmd
            .current_dir(work.path())
            .args(["run", "--no-mcp", "--runtime-dir", given]);
        let daemon = DaemonProcess::spawn(daemon_cmd)?
            .wait_ready(&lock_path, Duration::from_secs(20))
            .await
            .unwrap_or_else(|failure| panic!("{given}: daemon did not become ready: {failure}"));

        let info = bookrack_session::peek_lock(&lock_path)?.expect("the lock is written");
        let recorded = info.control_sock.expect("the lock names a control socket");
        assert!(
            recorded.is_absolute()
                && recorded
                    .components()
                    .all(|part| matches!(part, Component::RootDir | Component::Normal(_))),
            "{given}: recorded {}",
            recorded.display()
        );
        assert_eq!(
            recorded.parent().expect("a parent").canonicalize()?,
            runtime_dir.canonicalize()?,
            "{given}: recorded {}",
            recorded.display()
        );

        for args in [&["status"][..], &["quit"][..]] {
            let out = tokio::process::Command::from(
                bookrack_cmd!(sandbox)
                    .runtime_dir(&runtime_dir)
                    .data_dir(data_root.path())
                    .build(),
            )
            .args(args)
            .output()
            .await?;
            assert_eq!(
                out.status.code(),
                Some(0),
                "{given} {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        daemon
            .exit_within(Duration::from_secs(10))
            .await
            .unwrap_or_else(|failure| panic!("{given}: daemon did not exit: {failure}"));
    }
    Ok(())
}
