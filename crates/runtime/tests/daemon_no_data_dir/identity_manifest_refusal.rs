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
//! skips it — or, with nothing else to serve, refuses — and the runtime
//! `library.mount` refuses, before anything is locked or written.
//!
//! A skip is recorded while the daemon goes on serving, so its reason
//! reads like the runtime mount that repairs it, word for word; a
//! refused bring-up's reason says to start the daemon again.
//!
//! The embedder probe the daemon performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

use std::path::Path;

use bookrack_config::{
    LibraryEntryFields, LibraryKind, new_manifest, upsert_library_entry_claiming_default,
    write_manifest,
};
use bookrack_core::{Explain, Problem};
use bookrack_ops::registry::UnmountedLibrary;
use bookrack_runtime::backend_probe::PreflightRefusal;
use bookrack_runtime::unmounted::NoServableLibrary;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

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

/// Start under `name` and return the error bring-up refused with,
/// shutting the daemon down first when it came up instead.
async fn bring_up_error(name: &str, root: &Path) -> Result<eyre::Report> {
    let runtime_root = tempfile::tempdir()?;
    match DaemonRuntime::start(headless(name, runtime_root.path())).await {
        Ok(runtime) => {
            let shutdown_tx = runtime.shutdown_tx.clone();
            let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
            let _ = shutdown_tx.send(());
            runtime.run_until_shutdown(None, repl_handle).await?;
            Err(eyre!(
                "bring-up on {} came up and left {:?} behind; expected a refusal",
                root.display(),
                contents(root)
            ))
        }
        Err(e) => Ok(e),
    }
}

/// Start under `name` and return the refusal, shutting the daemon down
/// first when it came up instead.
async fn refusal_of(name: &str, root: &Path) -> Result<(String, Problem)> {
    let err = bring_up_error(name, root).await?;
    err.downcast_ref::<PreflightRefusal>()
        .map(|refusal| (refusal.library.clone(), refusal.problem.clone()))
        .ok_or_else(|| eyre!("bring-up must refuse with a PreflightRefusal: {err:#}"))
}

/// The record bring-up kept for `name`, or a failure naming what it
/// kept instead.
fn skip_record(runtime: &DaemonRuntime, name: &str) -> Result<UnmountedLibrary> {
    let unmounted = runtime.registry.unmounted()?;
    unmounted
        .iter()
        .find(|library| library.name() == name)
        .cloned()
        .with_context(|| {
            format!(
                "{name} is not recorded as unmounted; recorded: {:?}",
                unmounted.iter().map(|u| u.name()).collect::<Vec<_>>()
            )
        })
}

/// Ask the running daemon to mount `name` and require the refusal to be
/// the skip `record` word for word — summary, detail, hint, and
/// `retryable` — under invalid params, naming the mount command, with
/// `root` still holding exactly `left`. Shuts the daemon down.
async fn mount_answers_with_the_skip(
    runtime: DaemonRuntime,
    name: &'static str,
    record: UnmountedLibrary,
    root: &Path,
    left: Vec<String>,
) -> Result<()> {
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let root = root.to_path_buf();
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "library.mount", "params": { "name": name },
        });
        send(&mut w, &req.to_string()).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["error"]["code"], -32602, "{resp}");
        assert_eq!(
            resp["error"]["message"].as_str(),
            Some(record.reason().summary.as_str()),
            "the mount's summary is the skip's summary: {resp}"
        );
        assert_eq!(
            resp["error"]["data"],
            serde_json::to_value(&record.reason().data)?,
            "the mount's detail, hint, and retryable are the skip's, word for word: {resp}"
        );
        let mount = format!("`bookrack libraries mount {name}`");
        assert!(
            resp["error"]["data"]["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains(&mount)),
            "the hint must name {mount}: {resp}"
        );
        assert_eq!(
            contents(&root),
            left,
            "neither the skip nor the refused mount may write to the root"
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
    let hint = problem.data.hint.as_deref().unwrap_or_default();
    assert!(
        hint.starts_with("Reconnect the volume"),
        "the hint must say to reconnect the volume first: {problem:?}"
    );
    assert!(
        hint.ends_with("then start the daemon again."),
        "a bring-up that refused must say to start the daemon again: {problem:?}"
    );
    assert_eq!(
        contents(&alpha),
        Vec::<String>::new(),
        "a refused bring-up must leave nothing in the mount point"
    );
    Ok(())
}

/// Beside a library that comes up, a bare root under a cached uuid is
/// skipped rather than served, nothing is written to it, and the skip
/// reads as the mount that repairs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bare_root_beside_a_served_library_is_skipped_with_the_mounts_wording() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-served");
    let beta = sandbox.data_root("beta-mount");
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
    register(
        sandbox,
        "beta",
        &beta,
        &new_manifest("beta", LibraryKind::Test, None).uuid,
    );
    let runtime_root = tempfile::tempdir()?;

    let runtime = DaemonRuntime::start(headless("alpha", runtime_root.path())).await?;
    assert!(
        runtime.registry.get(Some("alpha")).is_ok(),
        "alpha is served"
    );
    let record = skip_record(&runtime, "beta")?;
    assert!(
        record.reason().summary.contains("no identity manifest"),
        "{:?}",
        record.reason()
    );
    assert_eq!(
        contents(&beta),
        Vec::<String>::new(),
        "a skipped bring-up must leave nothing in the mount point"
    );

    mount_answers_with_the_skip(runtime, "beta", record, &beta, Vec::new()).await
}

/// Beside a library that comes up, a root whose manifest records
/// another uuid is skipped, its manifest is left byte for byte, and the
/// skip reads as the mount that repairs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_swapped_root_beside_a_served_library_is_skipped_with_the_mounts_wording() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-served");
    let beta = sandbox.data_root("beta-swapped");
    sandbox.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
    let present = new_manifest("other", LibraryKind::Test, None);
    write_manifest(&beta, &present)?;
    let manifest_path = beta.join(bookrack_config::MANIFEST_FILENAME);
    let manifest_bytes = std::fs::read(&manifest_path)?;
    register(
        sandbox,
        "beta",
        &beta,
        &new_manifest("beta", LibraryKind::Test, None).uuid,
    );
    let runtime_root = tempfile::tempdir()?;

    let runtime = DaemonRuntime::start(headless("alpha", runtime_root.path())).await?;
    assert!(
        runtime.registry.get(Some("alpha")).is_ok(),
        "alpha is served"
    );
    let record = skip_record(&runtime, "beta")?;
    assert!(
        record.reason().summary.contains("another uuid"),
        "{:?}",
        record.reason()
    );

    mount_answers_with_the_skip(
        runtime,
        "beta",
        record,
        &beta,
        vec![bookrack_config::MANIFEST_FILENAME.to_string()],
    )
    .await?;
    assert_eq!(
        std::fs::read(&manifest_path)?,
        manifest_bytes,
        "the manifest found at the root must be left as it was"
    );
    Ok(())
}

/// With no library to serve, the refusal's reasons say to start the
/// daemon again — not to mount through a daemon that is not running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_bare_roots_refuse_bring_up_saying_to_start_again() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-mount");
    let beta = sandbox.data_root("beta-mount");
    for (name, root) in [("alpha", &alpha), ("beta", &beta)] {
        register(
            sandbox,
            name,
            root,
            &new_manifest(name, LibraryKind::Test, None).uuid,
        );
    }

    let err = bring_up_error("alpha", &alpha).await?;

    let refusal = err
        .downcast_ref::<NoServableLibrary>()
        .ok_or_else(|| eyre!("two skips must refuse as NoServableLibrary: {err:#}"))?;
    let names: Vec<&str> = refusal.libraries.iter().map(|l| l.name()).collect();
    assert_eq!(names, ["alpha", "beta"]);
    for library in &refusal.libraries {
        let hint = library.reason().data.hint.as_deref().unwrap_or_default();
        assert!(
            hint.ends_with("then start the daemon again."),
            "{}'s reason in a refused bring-up must say to start again: {hint}",
            library.name()
        );
    }
    let hint = refusal.explain().data.hint.unwrap_or_default();
    assert!(
        !hint.contains("libraries mount"),
        "a refused bring-up has no daemon to mount through: {hint}"
    );
    for root in [&alpha, &beta] {
        assert_eq!(
            contents(root),
            Vec::<String>::new(),
            "a refused bring-up must leave nothing in the mount point"
        );
    }
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
