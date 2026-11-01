// SPDX-License-Identifier: Apache-2.0

//! A registered library that cannot come up is skipped, not fatal.
//!
//! Bring-up resolves, locks, preflights, and opens each registered
//! library on its own: one that fails at any of those steps is recorded
//! with its reason, its data-root lock is released, its queued jobs are
//! failed with the reason, and a call naming it is refused with the
//! reason and the mount command. The daemon stays up on the libraries
//! that did come up. Only when no candidate is left does bring-up
//! refuse: with the one cause it had, or naming every library when it
//! had several.
//!
//! Each test seeds its own registry; the registry env is pinned to a
//! per-binary tempdir, so none touches the user's registry file. The
//! embedder probe is answered by `bookrack_test_support::EmbedStub`, so
//! no Ollama daemon is required, and no test declares a reranker on a
//! library that comes up, so no `llama-server` is spawned.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use bookrack_catalog::{Catalog, SCHEMA_VERSION};
use bookrack_config::{LibraryKind, ManifestIdentitySeed, set_manifest_index_profile};
use bookrack_core::Explain;
use bookrack_core::queue::{JobState, QueueState};
use bookrack_ops::registry::RegistryError;
use bookrack_runtime::unmounted::NoServableLibrary;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_session::RootLock;
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, connect, join_with_deadline, recv, send};

const INVALID_LIBRARY: i64 = -32010;

/// A profile whose embed section matches the built-in default, so the
/// shared stub answers for it, carrying `reranker_section` verbatim.
fn profile_toml(name: &str, model: &str, reranker_section: &str) -> String {
    format!(
        r#"schema_version = 1
name = "{name}"
description = "Test profile."

[embed]
backend = "ollama"
model = "{model}"
dim = 1024

[ann]
kind = "ivf-pq"
num_partitions = 256
num_sub_vectors = 128
num_bits = 8
nprobes = 16
refine_factor = 10

{reranker_section}
"#
    )
}

const STUB_MODEL: &str = "qwen3-embedding:0.6b";

const NO_RERANKER: &str = r#"[reranker]
kind = "none""#;

const CROSS_ENCODER: &str = r#"[reranker]
kind = "cross-encoder"
backend = "llama-server"
model = "Qwen3-Reranker-0.6B"
top_k_in = 50
top_k_out = 10"#;

fn world() -> &'static Sandbox {
    process_env(ProcessEnv::daemon().without_data_dir())
}

/// Write an index profile the sandbox's daemon resolves by name.
fn write_profile(sandbox: &Sandbox, name: &str, model: &str, reranker_section: &str) {
    let dir = sandbox.path().join("index-profiles");
    std::fs::create_dir_all(&dir).expect("profile directory");
    std::fs::write(
        dir.join(format!("{name}.toml")),
        profile_toml(name, model, reranker_section),
    )
    .expect("write profile");
}

/// Declare `profile` in the manifest of the library at `root`.
fn declare_profile(root: &Path, name: &str, profile: &str) {
    set_manifest_index_profile(
        root,
        Some(profile),
        ManifestIdentitySeed {
            name,
            kind: LibraryKind::Test,
            description: None,
            registered_uuid: None,
        },
    )
    .expect("declare the library's index profile");
}

/// Give `root` a catalog whose schema is one ahead of this build, so
/// the handle's preflight refuses it as `SchemaTooNew`.
fn break_catalog(root: &Path) {
    let db = root.join("catalog.db");
    drop(Catalog::open(&db).expect("seed catalog"));
    bookrack_dbkit::open_production(&db)
        .expect("open catalog")
        .pragma_update(None, "user_version", i64::from(SCHEMA_VERSION) + 1)
        .expect("advance user_version");
}

fn repair_catalog(root: &Path) {
    bookrack_dbkit::open_production(&root.join("catalog.db"))
        .expect("open catalog")
        .pragma_update(None, "user_version", i64::from(SCHEMA_VERSION))
        .expect("restore user_version");
}

async fn start(library: &str, runtime_root: &Path, worker: bool) -> Result<DaemonRuntime> {
    let mut opts = RuntimeOpts::headless(None, Some(library.to_string()));
    opts.no_mcp = true;
    opts.spawn_queue_worker = worker;
    opts.runtime_dir = Some(runtime_root.to_path_buf());
    DaemonRuntime::start(opts).await
}

/// Stop a runtime no driver is talking to.
async fn stop(runtime: DaemonRuntime) -> Result<()> {
    let shutdown_tx = runtime.shutdown_tx.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let _ = shutdown_tx.send(());
    runtime.run_until_shutdown(None, repl_handle).await
}

async fn call(
    reader: &mut Reader,
    w: &mut Writer,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    send(w, &req.to_string()).await?;
    recv(reader).await
}

/// The record bring-up kept for `name`, or a failure naming what it
/// kept instead.
fn skip_record(
    runtime: &DaemonRuntime,
    name: &str,
) -> Result<bookrack_ops::registry::UnmountedLibrary> {
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

/// The lock a test holds on a root, so the daemon finds it taken.
fn hold_root(root: &Path) -> RootLock {
    RootLock::acquire(root, std::process::id(), "test").expect("hold the root lock")
}

/// The queue document path the daemon under test reads.
fn queue_path() -> Result<PathBuf> {
    Ok(bookrack_config::daemon_state_dir()
        .map_err(|e| eyre!("resolve the daemon state directory: {e}"))?
        .join("queue.json"))
}

fn unnamed_row(id: &str, library: &str, state: &str) -> Value {
    let terminal = !matches!(state, "pending" | "running");
    json!({
        "id": id,
        "library": library,
        "path": format!("/disk/nowhere/{id}.txt"),
        "priority": "normal",
        "force": false,
        "hold_for_metadata": false,
        "kind": "book",
        "intake_ocr": null,
        "audit_profile": null,
        "state": state,
        "queued_at": "2026-01-02T03:04:05Z",
        "started_at": if state == "pending" { Value::Null } else { json!("2026-01-02T03:04:06Z") },
        "finished_at": if terminal { json!("2026-01-02T03:05:05Z") } else { Value::Null },
        "error": if state == "failed" { json!("earlier failure") } else { Value::Null },
        "merged_into": null,
    })
}

fn seed_queue(rows: Vec<Value>) -> Result<PathBuf> {
    let path = queue_path()?;
    std::fs::create_dir_all(path.parent().expect("state dir"))?;
    let document = json!({
        "schema_version": bookrack_core::queue::QUEUE_SCHEMA_VERSION,
        "paused": true,
        "jobs": rows,
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&document)?)?;
    Ok(path)
}

fn job<'a>(state: &'a QueueState, id: &str) -> &'a bookrack_core::queue::QueueJob {
    state
        .jobs
        .iter()
        .find(|job| job.id == id)
        .unwrap_or_else(|| panic!("job {id} is not in the queue"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_library_whose_catalog_this_build_cannot_read_is_skipped() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    break_catalog(&beta);
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), false).await?;
    assert!(
        runtime.registry.get(Some("alpha")).is_ok(),
        "alpha is served"
    );
    match runtime.registry.get(Some("beta")) {
        Err(RegistryError::LibraryUnmounted { name, reason }) => {
            assert_eq!(name, "beta");
            assert!(
                reason
                    .data
                    .hint
                    .as_deref()
                    .unwrap_or("")
                    .contains("bookrack"),
                "the skip carries the catalog's own hint: {reason:?}"
            );
        }
        Err(other) => panic!("beta must be recorded as unmounted, got {other:?}"),
        Ok(_) => panic!("beta must be recorded as unmounted, got a served handle"),
    }
    let record = skip_record(&runtime, "beta")?;
    assert_eq!(record.data_dir(), beta.as_path());
    // The root lock a skipped library took is released.
    drop(hold_root(&beta));
    stop(runtime).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_library_whose_root_another_process_holds_is_skipped() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let _held = hold_root(&beta);
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), false).await?;
    assert!(runtime.registry.get(Some("alpha")).is_ok());
    let record = skip_record(&runtime, "beta")?;
    assert!(
        record.reason().summary.contains("lock"),
        "the reason names the lock: {}",
        record.reason().summary
    );
    stop(runtime).await
}

/// The library the daemon is started under is skipped before any
/// handle is built; the daemon comes up on the other one and the
/// skipped root's lock, which this process had taken, is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skipped_primary_leaves_the_daemon_up() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    declare_profile(&beta, "beta", "ghost");
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("beta", runtime_root.path(), false).await?;
    assert_eq!(runtime.cfg.library(), Some("beta"));
    assert_eq!(runtime.registry.get(None)?.name(), "alpha");
    let record = skip_record(&runtime, "beta")?;
    assert!(
        record.reason().summary.contains("ghost") || record.reason().summary.contains("profile"),
        "{}",
        record.reason().summary
    );
    drop(hold_root(&beta));
    stop(runtime).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_root_on_another_library_is_skipped() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.path().join("roots").join("beta-missing");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), false).await?;
    assert!(runtime.registry.get(Some("alpha")).is_ok());
    let record = skip_record(&runtime, "beta")?;
    assert_eq!(record.data_dir(), beta.as_path());
    assert!(
        record
            .reason()
            .summary
            .contains("not an existing directory"),
        "{}",
        record.reason().summary
    );
    let hint = record.reason().data.hint.as_deref().unwrap_or("");
    assert!(hint.contains("Reconnect the volume"), "{hint}");
    assert!(hint.contains("bookrack libraries mount beta"), "{hint}");
    stop(runtime).await
}

/// Two entries on one root: the default claims it, the other is skipped
/// with a reason naming both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_duplicate_root_keeps_the_default() -> Result<()> {
    let sandbox = world();
    let shared = sandbox.data_root("shared-root");
    sandbox.write_registry_entries(
        Some("zeta"),
        &[("alpha", shared.as_path()), ("zeta", shared.as_path())],
    );
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("zeta", runtime_root.path(), false).await?;
    assert_eq!(runtime.registry.get(None)?.name(), "zeta");
    let record = skip_record(&runtime, "alpha")?;
    let detail = record.reason().data.detail.as_deref().unwrap_or("");
    assert!(detail.contains("\"zeta\""), "{detail}");
    assert!(
        detail.contains(&shared.display().to_string()),
        "the reason names the shared root: {detail}"
    );
    stop(runtime).await
}

/// Two libraries on one unusable backend are both skipped, each with the
/// backend's judgement; today only the first is named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn libraries_sharing_a_bad_embed_backend_are_all_named() -> Result<()> {
    let sandbox = world();
    write_profile(sandbox, "offmodel", "ghost-model:latest", NO_RERANKER);
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    let gamma = sandbox.data_root("gamma-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[
            ("alpha", alpha.as_path()),
            ("beta", beta.as_path()),
            ("gamma", gamma.as_path()),
        ],
    );
    declare_profile(&beta, "beta", "offmodel");
    declare_profile(&gamma, "gamma", "offmodel");
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), false).await?;
    assert!(runtime.registry.get(Some("alpha")).is_ok());
    for name in ["beta", "gamma"] {
        let record = skip_record(&runtime, name)?;
        assert!(
            record.reason().summary.contains("ghost-model"),
            "{name}: the reason names the backend's model: {}",
            record.reason().summary
        );
    }
    stop(runtime).await
}

/// The default was skipped and one healthy library remains: an unnamed
/// submit is refused rather than landing on it, and writes no row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unnamed_submit_is_refused_when_the_default_was_skipped() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let _held = hold_root(&alpha);
    let fixture_dir = tempfile::tempdir()?;
    let book = fixture_dir.path().join("synthetic-ledger.txt");
    std::fs::write(&book, "Chapter One\n\nA synthetic ledger.\n")?;
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("beta", runtime_root.path(), true).await?;
    skip_record(&runtime, "alpha")?;
    let queue_state = std::sync::Arc::clone(&runtime.queue_state);
    let queue_state_path = runtime.queue_state_path.clone();
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let paused = call(&mut reader, &mut w, 1, "queue.pause", Value::Null).await?;
        assert_eq!(paused["result"]["paused"], Value::Bool(true), "{paused}");

        let resp = call(
            &mut reader,
            &mut w,
            2,
            "ingest.submit",
            json!({ "paths": [book] }),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(INVALID_LIBRARY),
            "an unnamed submit must not land on the one healthy library: {resp}"
        );
        let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("--library"), "{resp}");
        let jobs = queue_state
            .lock()
            .map_err(|_| eyre!("queue lock poisoned"))?
            .jobs
            .len();
        assert_eq!(jobs, 0, "a refused submit writes no row");
        if let Ok(text) = std::fs::read_to_string(&queue_state_path) {
            let on_disk: Value = serde_json::from_str(&text)?;
            assert_eq!(on_disk["jobs"].as_array().map(Vec::len), Some(0), "{text}");
        }

        let resp = call(
            &mut reader,
            &mut w,
            3,
            "ingest.submit",
            json!({ "paths": [book], "library": "beta" }),
        )
        .await?;
        assert!(
            resp["result"]["job_ids"]
                .as_array()
                .is_some_and(|ids| !ids.is_empty()),
            "a submit naming the healthy library succeeds: {resp}"
        );

        let _ = call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// Rows that name no library are pinned to the registry default, and
/// nothing else about them changes; a finished row keeps its history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinning_an_unnamed_job_changes_only_its_library() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let rows = vec![
        unnamed_row("0a000000-0000-7000-8000-000000000001", "", "pending"),
        unnamed_row("0b000000-0000-7000-8000-000000000002", "", "failed"),
        unnamed_row("0c000000-0000-7000-8000-000000000003", "", "done"),
    ];
    let seeded: Vec<bookrack_core::queue::QueueJob> = rows
        .iter()
        .map(|row| serde_json::from_value(row.clone()).expect("seeded row parses"))
        .collect();
    seed_queue(rows)?;
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("beta", runtime_root.path(), true).await?;
    {
        let state = runtime
            .queue_state
            .lock()
            .map_err(|_| eyre!("queue lock poisoned"))?;
        for (seeded, expected_library) in seeded.iter().zip(["alpha", "alpha", ""]) {
            let pinned = job(&state, &seeded.id);
            assert_eq!(pinned.library, expected_library, "{}", seeded.id);
            let mut expected = seeded.clone();
            expected.library = expected_library.to_string();
            assert_eq!(
                *pinned, expected,
                "{}: only `library` may change",
                seeded.id
            );
        }
    }
    // On disk too.
    let text = std::fs::read_to_string(queue_path()?)?;
    let on_disk: QueueState = serde_json::from_str(&text)?;
    assert_eq!(job(&on_disk, &seeded[0].id).library, "alpha");
    assert_eq!(job(&on_disk, &seeded[2].id).library, "");

    // The retry of a pinned failed row carries the name along.
    let sock = runtime.control_sock.path.clone();
    let failed_id = seeded[1].id.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "queue.retry",
            json!({ "job_id": failed_id }),
        )
        .await?;
        assert!(resp["error"].is_null(), "{resp}");
        let listing = call(&mut reader, &mut w, 2, "queue.list", Value::Null).await?;
        let row = listing["result"]["jobs"]
            .as_array()
            .and_then(|jobs| {
                jobs.iter()
                    .find(|j| j["id"] == Value::String(failed_id.clone()))
            })
            .with_context(|| format!("retried row not listed: {listing}"))?;
        assert_eq!(row["state"], "pending", "{row}");
        assert_eq!(row["library"], "alpha", "{row}");
        let _ = call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// With no registry default, an unnamed row is pinned to the library
/// the daemon came up under even when that library was skipped: it then
/// fails with the skip, rather than moving to the one healthy library.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unnamed_jobs_pinned_to_a_skipped_primary_fail_rather_than_move() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        None,
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    declare_profile(&beta, "beta", "ghost");
    let id = "0d000000-0000-7000-8000-000000000004";
    seed_queue(vec![unnamed_row(id, "", "pending")])?;
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("beta", runtime_root.path(), true).await?;
    skip_record(&runtime, "beta")?;
    {
        let state = runtime
            .queue_state
            .lock()
            .map_err(|_| eyre!("queue lock poisoned"))?;
        let row = job(&state, id);
        assert_eq!(
            row.library, "beta",
            "the row stays with the bring-up library"
        );
        assert_eq!(row.state, JobState::Failed);
        assert!(
            row.problem
                .as_ref()
                .and_then(|problem| problem.data.hint.as_deref())
                .unwrap_or("")
                .contains("`bookrack libraries mount beta`"),
            "{:?}",
            row.problem
        );
        assert!(
            state.jobs.iter().all(|j| j.library != "alpha"),
            "no row moves to the healthy library"
        );
    }
    stop(runtime).await
}

/// Both libraries fail: the refusal names each, and each reason closes
/// with starting the daemon again rather than with a mount.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_library_coming_up_refuses_naming_each() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    break_catalog(&alpha);
    let _held = hold_root(&beta);
    let runtime_root = tempfile::tempdir()?;

    let err = match start("alpha", runtime_root.path(), false).await {
        Ok(runtime) => {
            stop(runtime).await?;
            panic!("bring-up with no servable library must refuse");
        }
        Err(err) => err,
    };
    let refusal = err
        .downcast_ref::<NoServableLibrary>()
        .unwrap_or_else(|| panic!("two skips must refuse as NoServableLibrary: {err:#}"));
    let names: Vec<&str> = refusal.libraries.iter().map(|l| l.name()).collect();
    assert_eq!(names, ["alpha", "beta"]);
    // A store this build cannot serve and a root another process holds
    // each close with starting the daemon again: no daemon is left to
    // take a mount.
    for library in &refusal.libraries {
        let hint = library.reason().data.hint.as_deref().unwrap_or_default();
        assert!(
            hint.ends_with("start the daemon again."),
            "{}'s reason in a refused bring-up must say to start again: {hint}",
            library.name()
        );
    }
    let merged = refusal.explain().data.hint.unwrap_or_default();
    assert!(
        !merged.contains("mount"),
        "a refused bring-up has no daemon to mount through: {merged}"
    );
    // Nothing of this process holds either root now.
    drop(hold_root(&alpha));
    Ok(())
}

/// A refused bring-up tells a library whose root is missing to start
/// the daemon again once the volume is back: no daemon is left to take
/// a `libraries mount`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_root_in_a_refused_bring_up_says_to_start_again() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.path().join("roots").join("beta-missing");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let _held = hold_root(&alpha);
    let runtime_root = tempfile::tempdir()?;

    let err = match start("alpha", runtime_root.path(), false).await {
        Ok(runtime) => {
            stop(runtime).await?;
            panic!("bring-up with no servable library must refuse");
        }
        Err(err) => err,
    };
    let refusal = err
        .downcast_ref::<NoServableLibrary>()
        .unwrap_or_else(|| panic!("two skips must refuse as NoServableLibrary: {err:#}"));
    let beta_reason = refusal
        .libraries
        .iter()
        .find(|library| library.name() == "beta")
        .map(|library| library.reason().clone())
        .unwrap_or_else(|| panic!("the refusal names beta: {err:#}"));
    assert!(
        beta_reason.summary.contains("not an existing directory"),
        "{beta_reason:?}"
    );
    let hint = beta_reason.data.hint.as_deref().unwrap_or_default();
    assert!(hint.starts_with("Reconnect the volume"), "{hint}");
    assert!(
        hint.ends_with(" Then start the daemon again."),
        "beta's reason in a refused bring-up must say to start again: {hint}"
    );
    let merged = refusal.explain().data.hint.unwrap_or_default();
    assert!(
        !merged.contains("libraries mount beta"),
        "a refused bring-up has no daemon to mount through: {merged}"
    );
    Ok(())
}

/// Queued work against a skipped library is failed at bring-up with the
/// reason, the mount and the retry in its problem's hint and not in its
/// error; another library's rows are untouched; the file agrees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_against_a_skipped_library_fail_at_bring_up() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    break_catalog(&beta);
    let beta_pending = "0e000000-0000-7000-8000-000000000005";
    let beta_running = "0f000000-0000-7000-8000-000000000006";
    let alpha_pending = "10000000-0000-7000-8000-000000000007";
    seed_queue(vec![
        unnamed_row(beta_pending, "beta", "pending"),
        unnamed_row(beta_running, "beta", "running"),
        unnamed_row(alpha_pending, "alpha", "pending"),
    ])?;
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), true).await?;
    {
        let state = runtime
            .queue_state
            .lock()
            .map_err(|_| eyre!("queue lock poisoned"))?;
        for id in [beta_pending, beta_running] {
            let row = job(&state, id);
            assert_eq!(row.state, JobState::Failed, "{id}");
            // The error states the fact and groups the row; the repair
            // is the problem's hint alone, so a failure listing that
            // prints both says the mount once.
            let error = row.error.as_deref().unwrap_or("");
            assert!(
                error.contains("\"beta\" was not mounted at bring-up"),
                "{id}: {error}"
            );
            assert!(!error.contains("libraries mount"), "{id}: {error}");
            let hint = row
                .problem
                .as_ref()
                .and_then(|problem| problem.data.hint.as_deref())
                .unwrap_or("");
            assert_eq!(
                hint.matches("`bookrack libraries mount beta`").count(),
                1,
                "{id}: {hint}"
            );
            assert!(
                hint.ends_with("Once it is mounted, run `bookrack queue retry`."),
                "{id}: {hint}"
            );
        }
        assert_eq!(job(&state, alpha_pending).state, JobState::Pending);
    }
    let on_disk: QueueState = serde_json::from_str(&std::fs::read_to_string(queue_path()?)?)?;
    assert_eq!(job(&on_disk, beta_pending).state, JobState::Failed);
    assert_eq!(job(&on_disk, alpha_pending).state, JobState::Pending);
    stop(runtime).await
}

/// A retry leaves a row bring-up failed against a skipped library as
/// bring-up wrote it: a state retry holds it and says so, and a retry
/// naming it is refused with the mount step, while a row against a
/// served library is flipped as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retry_keeps_the_bring_up_failure_on_a_skipped_librarys_row() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    break_catalog(&beta);
    let beta_pending = "11000000-0000-7000-8000-000000000008";
    let alpha_failed = "12000000-0000-7000-8000-000000000009";
    seed_queue(vec![
        unnamed_row(beta_pending, "beta", "pending"),
        unnamed_row(alpha_failed, "alpha", "failed"),
    ])?;
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), true).await?;
    let failed_at_bring_up = {
        let state = runtime
            .queue_state
            .lock()
            .map_err(|_| eyre!("queue lock poisoned"))?;
        job(&state, beta_pending).clone()
    };
    assert_eq!(failed_at_bring_up.state, JobState::Failed);
    assert!(
        failed_at_bring_up.problem.is_some(),
        "{failed_at_bring_up:?}"
    );
    let queue_state = std::sync::Arc::clone(&runtime.queue_state);
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "queue.retry",
            json!({ "state": "failed" }),
        )
        .await?;
        assert_eq!(resp["result"]["retried"], json!([alpha_failed]), "{resp}");
        assert_eq!(
            resp["result"]["held"],
            json!([{ "id": beta_pending, "library": "beta" }]),
            "{resp}"
        );

        let resp = call(
            &mut reader,
            &mut w,
            2,
            "queue.retry",
            json!({ "job_id": beta_pending }),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(INVALID_LIBRARY),
            "{resp}"
        );
        let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("`bookrack libraries mount beta`"), "{resp}");

        {
            let state = queue_state
                .lock()
                .map_err(|_| eyre!("queue lock poisoned"))?;
            assert_eq!(
                *job(&state, beta_pending),
                failed_at_bring_up,
                "neither retry touches the row bring-up failed"
            );
            assert_eq!(job(&state, alpha_failed).state, JobState::Pending);
        }

        let _ = call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// A named call to a skipped library is refused with the reason and the
/// mount command, and mounting the repaired library clears the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_named_call_to_a_skipped_library_says_why_and_a_repair_clears_it() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    break_catalog(&beta);
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), false).await?;
    let registry = std::sync::Arc::clone(&runtime.registry);
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let beta_root = beta.clone();
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "library.info",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(INVALID_LIBRARY),
            "{resp}"
        );
        let message = resp["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("not mounted"), "{resp}");
        let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("bookrack libraries mount beta"), "{resp}");

        repair_catalog(&beta_root);
        let resp = call(
            &mut reader,
            &mut w,
            2,
            "library.mount",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(resp["result"]["ok"], Value::Bool(true), "{resp}");
        assert!(
            registry.unmounted()?.is_empty(),
            "a mounted library leaves the unmounted record"
        );
        let resp = call(
            &mut reader,
            &mut w,
            3,
            "library.info",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(resp["result"]["library_name"], "beta", "{resp}");

        let _ = call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// The reranker reference is the default when it comes up, else the
/// first library that did: with the default skipped, a library that
/// disagrees with the new reference is skipped — and no reranker is
/// started for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_default_moves_the_reranker_reference() -> Result<()> {
    let sandbox = world();
    write_profile(sandbox, "plain", STUB_MODEL, NO_RERANKER);
    write_profile(sandbox, "reranked", STUB_MODEL, CROSS_ENCODER);
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    let gamma = sandbox.data_root("gamma-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[
            ("alpha", alpha.as_path()),
            ("beta", beta.as_path()),
            ("gamma", gamma.as_path()),
        ],
    );
    declare_profile(&beta, "beta", "plain");
    declare_profile(&gamma, "gamma", "reranked");
    let _held = hold_root(&alpha);
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("beta", runtime_root.path(), false).await?;
    assert!(runtime.registry.get(Some("beta")).is_ok());
    skip_record(&runtime, "alpha")?;
    let record = skip_record(&runtime, "gamma")?;
    let detail = record.reason().data.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("\"beta\"") && detail.contains("\"gamma\""),
        "{detail}"
    );
    stop(runtime).await
}

/// A runtime mount of a library that is still broken refuses with the
/// same reason and hint bring-up skipped it for: a store this build
/// cannot serve is `-32018` with the store's own hint closed by the
/// mount command, and a root that is not there is `-32018` with the
/// volume and the mount command in the hint. Either way the command is
/// named once, and a read naming the library carries the same hint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_runtime_mount_of_a_still_broken_library_refuses_with_the_skips_wording() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    let gamma = sandbox.path().join("roots").join("gamma-missing");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[
            ("alpha", alpha.as_path()),
            ("beta", beta.as_path()),
            ("gamma", gamma.as_path()),
        ],
    );
    break_catalog(&beta);
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), false).await?;
    let beta_skip = skip_record(&runtime, "beta")?;
    let gamma_skip = skip_record(&runtime, "gamma")?;
    let beta_hint = beta_skip.reason().data.hint.clone().unwrap_or_default();
    assert!(
        beta_hint.ends_with(" Then run `bookrack libraries mount beta`."),
        "the record closes the store's own hint with the mount: {beta_hint}"
    );
    assert_eq!(
        beta_hint.matches("libraries mount").count(),
        1,
        "{beta_hint}"
    );
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let resp = call(
            &mut reader,
            &mut w,
            1,
            "library.mount",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32018),
            "a store this build cannot serve is state-unusable: {resp}"
        );
        assert_eq!(
            resp["error"]["data"]["hint"].as_str(),
            Some(beta_hint.as_str()),
            "the mount's hint is the skip's hint, word for word: {resp}"
        );
        let resp = call(
            &mut reader,
            &mut w,
            3,
            "library.info",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(INVALID_LIBRARY),
            "{resp}"
        );
        assert_eq!(
            resp["error"]["data"]["hint"].as_str(),
            Some(beta_hint.as_str()),
            "a call naming the library carries the skip's hint as it is: {resp}"
        );

        let resp = call(
            &mut reader,
            &mut w,
            2,
            "library.mount",
            json!({ "name": "gamma" }),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32018),
            "a missing root is state-unusable: {resp}"
        );
        let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
        assert_eq!(
            Some(hint),
            gamma_skip.reason().data.hint.as_deref(),
            "the mount's hint is the skip's hint, word for word: {resp}"
        );
        assert!(hint.contains("Reconnect the volume"), "{resp}");
        assert_eq!(
            hint.matches("bookrack libraries mount gamma").count(),
            1,
            "{resp}"
        );

        let _ = call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// A root another process still holds is skipped with a hint that
/// closes with the mount command once; a runtime mount while it is
/// still held refuses as `-32017` with that hint word for word, and a
/// read naming the library carries it as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_root_says_mount_once_on_the_record_the_mount_and_a_read() -> Result<()> {
    let sandbox = world();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let held = hold_root(&beta);
    let runtime_root = tempfile::tempdir()?;

    let runtime = start("alpha", runtime_root.path(), false).await?;
    let record = skip_record(&runtime, "beta")?;
    let hint = record.reason().data.hint.clone().unwrap_or_default();
    assert_eq!(
        hint.matches("`bookrack libraries mount beta`").count(),
        1,
        "the record names the mount command once: {hint}"
    );
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "library.mount",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32017),
            "a held root is backend-unavailable: {resp}"
        );
        assert_eq!(
            resp["error"]["data"]["hint"].as_str(),
            Some(hint.as_str()),
            "the mount's hint is the skip's hint, word for word: {resp}"
        );
        let resp = call(
            &mut reader,
            &mut w,
            2,
            "library.info",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(INVALID_LIBRARY),
            "{resp}"
        );
        assert_eq!(
            resp["error"]["data"]["hint"].as_str(),
            Some(hint.as_str()),
            "a call naming the library carries the skip's hint as it is: {resp}"
        );

        let _ = call(&mut reader, &mut w, 99, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });
    join_with_deadline(runtime, repl_handle, driver).await?;
    drop(held);
    Ok(())
}
