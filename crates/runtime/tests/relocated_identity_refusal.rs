// SPDX-License-Identifier: Apache-2.0

//! Bring-up on a path-class root whose manifest identity the registry
//! records at another path.
//!
//! The two directories are one library by identity and two places on
//! disk. Serving the selected one under the registered name would
//! redirect the daemon to the recorded root; serving it anonymously
//! would run one identity in two places. The daemon refuses instead,
//! and names both roots and the verb that resolves it.

#![cfg(unix)]

mod common;

use bookrack_config::{
    LibraryEntryFields, LibraryKind, MANIFEST_FILENAME, new_manifest,
    upsert_library_entry_claiming_default, write_manifest,
};
use bookrack_runtime::DaemonRuntime;
use bookrack_runtime::backend_probe::PreflightRefusal;
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::Result;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_whose_identity_is_registered_elsewhere_refuses_bring_up() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let runtime_root = tempfile::tempdir()?;

    // `alpha` is registered at `orig` with the uuid its manifest
    // carries; `copy` holds a byte-identical manifest.
    let orig = sandbox.data_root("alpha-orig");
    let copy = sandbox.data_root("alpha-copy");
    let manifest = new_manifest("alpha", LibraryKind::Test, None);
    write_manifest(&orig, &manifest)?;
    std::fs::copy(orig.join(MANIFEST_FILENAME), copy.join(MANIFEST_FILENAME))?;
    upsert_library_entry_claiming_default(
        &sandbox.registry_path(),
        "alpha",
        &LibraryEntryFields {
            data_dir: orig.clone(),
            kind: LibraryKind::Test,
            description: None,
            index_profile: None,
            created_at: manifest.created_at.clone(),
            uuid: Some(manifest.uuid.clone()),
        },
    )?;

    let opts = common::build_opts(copy.clone(), runtime_root.path().to_path_buf(), false);
    let err = match DaemonRuntime::start(opts).await {
        Ok(runtime) => {
            let shutdown_tx = runtime.shutdown_tx.clone();
            let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
            let _ = shutdown_tx.send(());
            runtime.run_until_shutdown(None, repl_handle).await?;
            panic!(
                "bring-up on {} came up; expected a refusal naming both roots",
                copy.display()
            );
        }
        Err(e) => e,
    };

    let refusal = err
        .downcast_ref::<PreflightRefusal>()
        .unwrap_or_else(|| panic!("bring-up must refuse with a PreflightRefusal: {err:#}"));
    let detail = refusal
        .problem
        .data
        .detail
        .as_deref()
        .unwrap_or_else(|| panic!("the refusal carries no detail: {:?}", refusal.problem));
    for root in [&orig, &copy] {
        assert!(
            detail.contains(&root.display().to_string()),
            "the refusal must name {}: {detail}",
            root.display()
        );
    }
    assert!(
        refusal
            .problem
            .data
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("libraries add")),
        "the hint must name the verb that re-registers or re-identifies the root: {:?}",
        refusal.problem
    );
    Ok(())
}
