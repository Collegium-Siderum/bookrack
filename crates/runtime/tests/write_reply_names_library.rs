// SPDX-License-Identifier: Apache-2.0

//! A write that goes through the daemon's write mutex names, in its
//! success reply, the library it acted on.
//!
//! The name is the one the call resolved to: the explicit `library`
//! parameter, or the registry's default pointer at the moment of the
//! call. Two libraries: `alpha` is the registry default and the daemon
//! is brought up under `beta`, so the primary and the default differ —
//! which is what makes the source of an unnamed call's answer
//! observable. The embedder probe daemon bring-up performs is answered
//! by `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::ItemKind;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{ContextCompat, Result};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

/// The data roots of the two registered libraries.
struct Roots {
    alpha: PathBuf,
}

/// Isolate the process and seed a two-library registry: `alpha` (the
/// registry default) and `beta`, each with its own data root under the
/// sandbox.
fn world() -> &'static Roots {
    static ROOTS: OnceLock<Roots> = OnceLock::new();
    let sandbox: &Sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    ROOTS.get_or_init(|| {
        let alpha = sandbox.data_root("alpha-root");
        let beta = sandbox.data_root("beta-root");
        sandbox.write_registry_entries(
            Some("alpha"),
            &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
        );
        Roots { alpha }
    })
}

/// Bring the daemon up under `beta` while the registry default is
/// `alpha`.
async fn start_under_beta(runtime_root: &Path) -> Result<DaemonRuntime> {
    let mut opts = RuntimeOpts::headless(None, Some("beta".to_string()));
    opts.no_mcp = true;
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    assert_eq!(
        runtime.registry.get(None)?.name(),
        "alpha",
        "the fixture's default pointer has to differ from the primary \
         for this test to discriminate"
    );
    Ok(runtime)
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

/// The `result` of a call that must succeed.
async fn ok_result(
    reader: &mut Reader,
    w: &mut Writer,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let resp = call(reader, w, id, method, params).await?;
    assert!(resp["error"].is_null(), "{method} failed: {resp}");
    Ok(resp["result"].clone())
}

/// A planning write answers with its plan and with the library it was
/// planned against: the default pointer when the call names none, the
/// named library when it names one, and the moved pointer after
/// `library.set_default`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unnamed_write_lands_on_the_registry_default_and_says_so() -> Result<()> {
    world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start_under_beta(runtime_root.path()).await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let mut id = 0u64;

        // Control: the daemon itself reports the primary and the
        // default on different rows, so a wrong answer below is the
        // reply's doing and not the fixture's.
        id += 1;
        let status = ok_result(&mut reader, &mut w, id, "daemon.status", Value::Null).await?;
        assert_eq!(status["library"], "beta", "{status}");
        let served = status["served"]
            .as_array()
            .with_context(|| format!("status carries no served rows: {status}"))?;
        let default_row = served
            .iter()
            .find(|row| row["default"] == Value::Bool(true))
            .with_context(|| format!("no default row in served: {status}"))?;
        assert_eq!(default_row["name"], "alpha", "{status}");

        let plan = json!({ "dry_run": true });

        id += 1;
        let unnamed = ok_result(&mut reader, &mut w, id, "corpus.rebuild", plan.clone()).await?;
        assert_eq!(
            unnamed["library"], "alpha",
            "an unnamed write must name the registry default it landed on: {unnamed}"
        );
        assert!(
            unnamed["plan_id"].is_string(),
            "the reply keeps the fields it had: {unnamed}"
        );

        id += 1;
        let named = ok_result(
            &mut reader,
            &mut w,
            id,
            "corpus.rebuild",
            json!({ "dry_run": true, "library": "beta" }),
        )
        .await?;
        assert_eq!(
            named["library"], "beta",
            "a named write must name the library it was given: {named}"
        );

        id += 1;
        let flipped = ok_result(
            &mut reader,
            &mut w,
            id,
            "library.set_default",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(flipped["ok"], Value::Bool(true), "{flipped}");

        id += 1;
        let moved = ok_result(&mut reader, &mut w, id, "corpus.rebuild", plan).await?;
        assert_eq!(
            moved["library"], "beta",
            "an unnamed write must follow the default pointer: {moved}"
        );

        // Restore the registry default for any other test sharing the
        // sandbox.
        id += 1;
        let restored = ok_result(
            &mut reader,
            &mut w,
            id,
            "library.set_default",
            json!({ "name": "alpha" }),
        )
        .await?;
        assert_eq!(restored["ok"], Value::Bool(true), "{restored}");

        id += 1;
        let _ = call(&mut reader, &mut w, id, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}

/// A metadata write whose reply carried nothing but `ok` now carries
/// the library it acted on beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bare_ok_write_carries_the_library() -> Result<()> {
    let roots = world();
    let runtime_root = tempfile::tempdir()?;
    let runtime = start_under_beta(runtime_root.path()).await?;
    let book = Catalog::open(&roots.alpha.join("catalog.db"))?
        .register_intake(
            ItemKind::Book,
            &NewIntake::new("sha-write-reply").format("epub"),
        )?
        .into_intake()
        .intake_id;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let reply = ok_result(
            &mut reader,
            &mut w,
            1,
            "metadata.ack",
            json!({ "book": book, "reason": "the gap is known" }),
        )
        .await?;
        assert_eq!(reply, json!({ "ok": true, "library": "alpha" }));

        let _ = call(&mut reader, &mut w, 2, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}

/// A daemon brought up on a root selected by path, not by registry
/// name, serves it under the handle name `default` and reports that
/// name in a write reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_selected_root_is_reported_as_default() -> Result<()> {
    world();
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let reply = ok_result(
            &mut reader,
            &mut w,
            1,
            "corpus.rebuild",
            json!({ "dry_run": true }),
        )
        .await?;
        assert_eq!(reply["library"], "default", "{reply}");

        let _ = call(&mut reader, &mut w, 2, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
