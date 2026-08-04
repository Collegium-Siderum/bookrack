// SPDX-License-Identifier: Apache-2.0

//! Bring-up against a queue document a later binary wrote.
//!
//! The document spans libraries and lives in the daemon state
//! directory, so it is read once at bring-up before any library is
//! served. A document above this binary's `QUEUE_SCHEMA_VERSION` is
//! refused there, and the refusal has to reach the operator as the
//! typed error the CLI renders in three parts — a bring-up that failed
//! for an untyped reason exits 1 as a bug report instead.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use bookrack_runtime::queue::{QUEUE_SCHEMA_VERSION, QueueLoadError};
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::Result;

use crate::common::build_opts;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bring_up_refuses_a_newer_queue_document_and_leaves_it_on_disk() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;

    let queue_path = sandbox.daemon_state_dir().join("queue.json");
    let doc = format!(
        "{{\"schema_version\": {}, \"paused\": false, \"jobs\": [], \
         \"future_field\": \"keep me\"}}",
        QUEUE_SCHEMA_VERSION + 1
    );
    std::fs::write(&queue_path, doc.as_bytes())?;

    let err = match bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await
    {
        Ok(runtime) => {
            let shutdown_tx = runtime.shutdown_tx.clone();
            let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
            let _ = shutdown_tx.send(());
            runtime.run_until_shutdown(None, repl_handle).await?;
            panic!("bring-up served a queue document from a newer binary");
        }
        Err(e) => e,
    };

    // The front end branches on the type to pick its exit code and to
    // render the three parts, so the refusal has to survive bring-up's
    // error plumbing as itself.
    let refusal = err
        .downcast_ref::<QueueLoadError>()
        .unwrap_or_else(|| panic!("bring-up must refuse with a QueueLoadError: {err:#}"));
    assert!(
        matches!(
            refusal,
            QueueLoadError::SchemaTooNew { found, .. } if *found == QUEUE_SCHEMA_VERSION + 1
        ),
        "{refusal}"
    );

    assert_eq!(
        std::fs::read_to_string(&queue_path)?,
        doc,
        "a refused bring-up must leave the document exactly as it found it"
    );
    Ok(())
}
