// SPDX-License-Identifier: Apache-2.0

//! End-to-end bring-up + tear-down for [`DaemonRuntime`].
//!
//! Boots the runtime in the headless profile against a fresh tempdir
//! (so no preexisting catalog, corpus, queue state, or lock file
//! interferes), fires the shared shutdown broadcast immediately, and
//! checks the invariants the manual mandates: the session lock is
//! released for re-acquisition, and a missing queue file deserialises
//! to the empty state without ever materialising on disk.
//!
//! Ignored by default because [`bookrack_query::Library::open`] probes
//! the configured Ollama daemon for the embedding model's dimension;
//! CI without a stub Ollama would surface the probe failure as a test
//! failure rather than as a missing prerequisite.

use std::path::PathBuf;

use bookrack_config::LibrarySelection;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_session::{TtyLock, tty_lock_name};
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
#[ignore = "requires a reachable Ollama embedding daemon"]
async fn start_then_shutdown_releases_lock_and_skips_queue_file() -> Result<()> {
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let data_path = data_root.path().to_path_buf();
    let runtime_path = runtime_root.path().to_path_buf();

    let runtime = DaemonRuntime::start(build_opts(data_path.clone(), runtime_path.clone())).await?;
    let shutdown_tx = runtime.shutdown_tx.clone();
    let lock_path = runtime.lock_path.clone();
    let queue_state_path = runtime.queue_state_path.clone();

    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let _ = shutdown_tx.send(());
    runtime.run_until_shutdown(None, repl_handle).await?;

    assert_eq!(
        lock_path.file_name().and_then(|s| s.to_str()),
        Some(tty_lock_name())
    );
    let reacquired = TtyLock::acquire(&lock_path, std::process::id(), "test", None);
    assert!(
        reacquired.is_ok(),
        "session lock should be released after run_until_shutdown",
    );
    drop(reacquired);

    assert!(
        !queue_state_path.exists(),
        "{} should not exist when the queue worker is disabled",
        queue_state_path.display()
    );

    Ok(())
}
