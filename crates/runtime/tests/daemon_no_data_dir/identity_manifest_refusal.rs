// SPDX-License-Identifier: Apache-2.0

//! A registry entry that caches a uuid is served only from a root whose
//! identity manifest carries that uuid.
//!
//! An empty mount point whose volume is not connected is a directory
//! that exists and holds nothing, so an existence check passes it and
//! the daemon would serve it as an empty library: searches answer
//! nothing, and an ingest writes a second store into the mount point,
//! where the volume covers it the moment it comes back. The manifest
//! is what tells that directory apart from the library, so bring-up
//! and the runtime `library.mount` both refuse before anything is
//! locked or written.
//!
//! The embedder probe the daemon performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

use std::path::Path;

use bookrack_config::{
    LibraryEntryFields, LibraryKind, new_manifest, upsert_library_entry_claiming_default,
    write_manifest,
};
use bookrack_core::Problem;
use bookrack_runtime::backend_probe::PreflightRefusal;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{Result, eyre};
use serde_json::Value;

use crate::common::{connect, join_with_deadline, recv, send};

/// Register `name` at `root` with `uuid` cached on the entry, the way
/// `libraries add` records a root whose manifest it wrote.
fn register(sandbox: &Sandbox, name: &str, root: &Path, uuid: &str) {
    upsert_library_entry_claiming_default(
        &sandbox.registry_path(),
        name,
        &LibraryEntryFields {
            data_dir: root.to_path_buf(),
            kind: LibraryKind::Test,
            description: None,
            index_profile: None,
            created_at: None,
            uuid: Some(uuid.to_string()),
        },
    )
    .expect("seed the registry entry");
}

/// The names of the directory's entries, so an assertion can say what
/// a refused bring-up left behind.
fn contents(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .expect("read the data root")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn headless(name: &str, runtime_root: &Path) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(None, Some(name.to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.to_path_buf());
    opts
}

/// Start under `name` and return the refusal, shutting the daemon down
/// first when it came up instead.
async fn refusal_of(name: &str, root: &Path) -> Result<(String, Problem)> {
    let runtime_root = tempfile::tempdir()?;
    let err = match DaemonRuntime::start(headless(name, runtime_root.path())).await {
        Ok(runtime) => {
            let shutdown_tx = runtime.shutdown_tx.clone();
            let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
            let _ = shutdown_tx.send(());
            runtime.run_until_shutdown(None, repl_handle).await?;
            return Err(eyre!(
                "bring-up on {} came up and left {:?} behind; expected a refusal",
                root.display(),
                contents(root)
            ));
        }
        Err(e) => e,
    };
    err.downcast_ref::<PreflightRefusal>()
        .map(|refusal| (refusal.library.clone(), refusal.problem.clone()))
        .ok_or_else(|| eyre!("bring-up must refuse with a PreflightRefusal: {err:#}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_root_without_its_manifest_refuses_bring_up() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    // The entry caches a uuid; the root is a bare directory, which is
    // what an unconnected volume's mount point looks like.
    let alpha = sandbox.data_root("alpha-mount");
    register(
        sandbox,
        "alpha",
        &alpha,
        &new_manifest("alpha", LibraryKind::Test, None).uuid,
    );
    assert!(
        contents(&alpha).is_empty(),
        "the fixture root has to start empty"
    );

    let (library, problem) = refusal_of("alpha", &alpha).await?;

    assert_eq!(library, "alpha", "{problem:?}");
    assert!(
        problem.summary.contains("identity manifest"),
        "the summary must say the root carries no identity manifest: {problem:?}"
    );
    assert!(
        problem
            .data
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("Reconnect the volume")),
        "the hint must say to reconnect the volume first: {problem:?}"
    );
    assert_eq!(
        contents(&alpha),
        Vec::<String>::new(),
        "a refused bring-up must leave nothing in the mount point"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_root_with_another_identity_refuses_bring_up() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    // The registry caches one uuid; the manifest at the root records
    // another: a different library's volume is mounted where this
    // one's belongs.
    let alpha = sandbox.data_root("alpha-swapped");
    let registered = new_manifest("alpha", LibraryKind::Test, None).uuid;
    let present = new_manifest("other", LibraryKind::Test, None);
    assert_ne!(registered, present.uuid);
    write_manifest(&alpha, &present)?;
    register(sandbox, "alpha", &alpha, &registered);

    let (library, problem) = refusal_of("alpha", &alpha).await?;

    assert_eq!(library, "alpha", "{problem:?}");
    let detail = problem
        .data
        .detail
        .as_deref()
        .unwrap_or_else(|| panic!("the refusal carries no detail: {problem:?}"));
    for uuid in [&registered, &present.uuid] {
        assert!(
            detail.contains(uuid.as_str()),
            "the refusal must name uuid {uuid} on its side: {detail}"
        );
    }
    assert_eq!(
        contents(&alpha),
        vec![bookrack_config::MANIFEST_FILENAME.to_string()],
        "a refused bring-up must add nothing to the root"
    );
    Ok(())
}

/// The runtime mount runs the same check: a daemon up on `alpha` is
/// asked to mount `beta`, whose entry caches a uuid and whose root is a
/// bare directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mount_refuses_a_registered_root_without_its_manifest() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-served");
    let beta = sandbox.data_root("beta-mount");
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(headless("alpha", runtime_root.path())).await?;
    register(
        sandbox,
        "beta",
        &beta,
        &new_manifest("beta", LibraryKind::Test, None).uuid,
    );

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.mount","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["error"]["code"], -32602, "{resp}");
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("identity manifest"),
            "the refusal must say the root carries no identity manifest: {resp}"
        );
        assert!(
            resp["error"]["data"]["hint"]
                .as_str()
                .unwrap_or_default()
                .contains("Reconnect the volume"),
            "the hint must say to reconnect the volume first: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":2,"method":"library.list"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(library_names(&resp)?, ["alpha"], "{resp}");
        assert_eq!(
            contents(&beta),
            Vec::<String>::new(),
            "a refused mount must leave nothing in the mount point"
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

fn library_names(resp: &Value) -> Result<Vec<&str>> {
    let entries = resp["result"]
        .as_array()
        .ok_or_else(|| eyre!("library.list did not return an array: {resp}"))?;
    Ok(entries.iter().filter_map(|e| e["name"].as_str()).collect())
}
