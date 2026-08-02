// SPDX-License-Identifier: Apache-2.0

//! Asserts that write-class JSON-RPC handlers surface caller-side input
//! failures as `INVALID_PARAMS` / `INVALID_LIBRARY` rather than
//! collapsing every downstream error to `INTERNAL_ERROR`.
//!
//! Boots a [`DaemonRuntime`] in the headless profile, then drives a
//! sequence of intentionally-bad write RPCs through a single
//! connection and asserts on `error.code` for each.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use std::path::Path;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::ItemKind;
use eyre::{Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer};
use crate::common::{build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

const INVALID_PARAMS: i64 = -32602;
const INVALID_LIBRARY: i64 = -32010;
const CONFIRMATION_REQUIRED: i64 = -32012;
const PLAN_TARGET_DRIFTED: i64 = -32016;

/// An id no intake in these fixtures can carry.
const PHANTOM: i64 = 999_999;

async fn rpc_code(
    writer: &mut Writer,
    reader: &mut Reader,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(i64, String)> {
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    send(writer, &frame.to_string()).await?;
    let resp = recv(reader).await?;
    let code = resp["error"]["code"]
        .as_i64()
        .ok_or_else(|| eyre!("expected error payload, got {resp}"))?;
    let message = resp["error"]["message"].as_str().unwrap_or("").to_string();
    Ok((code, message))
}

/// Issue one request and return the whole response frame.
async fn call(
    writer: &mut Writer,
    reader: &mut Reader,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    send(writer, &frame.to_string()).await?;
    recv(reader).await
}

/// Register one intake straight through the catalog API. Used to give
/// a fixture a catalog that exists and holds a known id, which is what
/// separates "this library has nothing in it" from "that id is not one
/// of the ids it has".
fn seed_intake(catalog_db: &Path, kind: ItemKind, sha: &str) -> Result<i64> {
    let mut catalog = Catalog::open(catalog_db)?;
    Ok(catalog
        .register_intake(kind, &NewIntake::new(sha).format("epub"))?
        .into_intake()
        .intake_id)
}

/// Take the `plan_id` out of a dry-run response.
fn plan_id(resp: &Value) -> Result<String> {
    resp["result"]["plan_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| eyre!("dry-run did not register a plan: {resp}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_handlers_surface_invalid_params_not_internal() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        // `library.set_default` with an unknown name has always returned
        // INVALID_LIBRARY (-32010); this guards the regression.
        let (code, _) = rpc_code(
            &mut w,
            &mut reader,
            1,
            "library.set_default",
            serde_json::json!({ "name": "ghost-library" }),
        )
        .await?;
        assert_eq!(code, INVALID_LIBRARY, "library.set_default unknown name");

        // `metadata.set` with an intake id that does not exist now
        // surfaces `OpsError::IntakeNotFound` as INVALID_PARAMS instead
        // of collapsing to INTERNAL_ERROR.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            2,
            "metadata.set",
            serde_json::json!({
                "book": 9_999_999_i64,
                "field": "title",
                "value": "anything",
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "metadata.set unknown intake: {msg}");

        // `metadata.set` with a field name outside the editable set
        // surfaces `OpsError::UnknownMetadataField` as INVALID_PARAMS.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            3,
            "metadata.set",
            serde_json::json!({
                "book": 1,
                "field": "definitely_not_a_real_field",
                "value": "x",
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "metadata.set unknown field: {msg}");

        // `corpus.rebuild` targeting an unknown book surfaces
        // `IngestError::UnknownIntake` (or `OpsError::IntakeNotFound`,
        // depending on where the lookup happens first) as INVALID_PARAMS.
        // The dry-run leg is what resolves the selector; the execute
        // leg would fail earlier, on the missing plan_id.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            4,
            "corpus.rebuild",
            serde_json::json!({
                "book": 9_999_999_i64,
                "dry_run": true,
                "yes": true,
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "corpus.rebuild unknown book: {msg}");

        // `vectors.reembed` targeting an unknown book surfaces
        // `IngestError::UnknownIntake` as INVALID_PARAMS.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            5,
            "vectors.reembed",
            serde_json::json!({
                "book": 9_999_999_i64,
                "yes": true,
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "vectors.reembed unknown book: {msg}");

        // `ingest.submit` with a path the extractor has no adapter for
        // is refused before a job is enqueued; mobi/azw3 carry a
        // conversion hint.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            6,
            "ingest.submit",
            serde_json::json!({ "paths": ["/tmp/book.mobi"] }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "ingest.submit mobi path: {msg}");
        assert!(
            msg.contains("convert to EPUB"),
            "ingest.submit mobi hint: {msg}"
        );

        // `glean.submit` runs the same extractor and applies the same
        // allowlist.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            7,
            "glean.submit",
            serde_json::json!({ "paths": ["/tmp/paper.mobi"] }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "glean.submit mobi path: {msg}");

        // `corpus.rebuild` execute leg without a plan_id is refused:
        // the two-phase protocol requires the dry-run's pinned plan.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            8,
            "corpus.rebuild",
            serde_json::json!({ "yes": true }),
        )
        .await?;
        assert_eq!(
            code, INVALID_PARAMS,
            "corpus.rebuild without plan_id: {msg}"
        );
        assert!(
            msg.contains("plan_id"),
            "the refusal must name the missing plan_id: {msg}"
        );

        // The yes gate runs ahead of the plan lookup: an unconfirmed
        // execute fails CONFIRMATION_REQUIRED even with a bogus plan.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            9,
            "corpus.rebuild",
            serde_json::json!({ "plan_id": "bogus-plan" }),
        )
        .await?;
        assert_eq!(
            code, CONFIRMATION_REQUIRED,
            "corpus.rebuild unconfirmed execute: {msg}"
        );

        // `library.fork` fails closed without the client-side
        // confirmation; nothing is copied or registered.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            10,
            "library.fork",
            serde_json::json!({ "new_name": "forked", "data_dir": "/tmp/fork-target" }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "library.fork without yes: {msg}");
        assert!(
            msg.contains("yes"),
            "the refusal must name the missing confirmation: {msg}"
        );

        // `library.fork` validates copy_mode before doing any work.
        let (code, msg) = rpc_code(
            &mut w,
            &mut reader,
            11,
            "library.fork",
            serde_json::json!({
                "new_name": "forked",
                "data_dir": "/tmp/fork-target",
                "yes": true,
                "copy_mode": "symlink",
            }),
        )
        .await?;
        assert_eq!(code, INVALID_PARAMS, "library.fork bad copy_mode: {msg}");
        assert!(
            msg.contains("copy_mode"),
            "the refusal must name the offending knob: {msg}"
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

/// `remove` against a library that has never been ingested into. The
/// fixture is exclusive on purpose: every other write RPC in this
/// suite reaches a read-write catalog open on its way through ops and
/// creates `catalog.db`, after which the absent-catalog leg can no
/// longer be reached. The remove dry-run has to be the first write
/// this data root sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_on_a_library_with_no_catalog_is_caller_input() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        for (id, method) in [(1, "remove"), (2, "papers.remove")] {
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"intake_id": 1, "dry_run": true, "yes": true}),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_PARAMS),
                "{method} on a library with no catalog: {resp}"
            );
            // The code alone cannot tell this leg from the unknown-id
            // leg — they share it. Naming the missing layer is what
            // proves the operator is being told to ingest something
            // rather than to correct the id.
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("catalog"),
                "{method} must name the layer the library is missing: {resp}"
            );
            let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
            assert!(
                !hint.is_empty(),
                "{method} must say what would give the library a catalog: {resp}"
            );
        }

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

/// Both remove surfaces, both selectors: a catalog that exists but
/// holds no such intake is caller input, not a handler fault.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_refuses_a_selector_the_catalog_cannot_resolve() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    // Both catalogs exist and hold one intake each, so a refusal below
    // can only be about the selector.
    let book_id = seed_intake(
        &data_root.path().join("catalog.db"),
        ItemKind::Book,
        "sha-book",
    )?;
    let paper_id = seed_intake(
        &data_root.path().join("papers_catalog.db"),
        ItemKind::Paper,
        "sha-paper",
    )?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        for (id, method) in [(1, "remove"), (2, "papers.remove")] {
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"intake_id": PHANTOM, "dry_run": true, "yes": true}),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_PARAMS),
                "{method} unknown intake id: {resp}"
            );
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("999999"),
                "{method} must name the id it could not resolve: {resp}"
            );
        }

        for (id, method) in [(3, "remove"), (4, "papers.remove")] {
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"sha": "no-such-sha", "dry_run": true, "yes": true}),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_PARAMS),
                "{method} unknown sha: {resp}"
            );
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("no-such-sha"),
                "{method} must name the hash it could not resolve: {resp}"
            );
        }

        // Positive control: the seeded ids still plan. Without it, a
        // guard that refused every selector would satisfy everything
        // above.
        for (id, method, intake_id) in [(5, "remove", book_id), (6, "papers.remove", paper_id)] {
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"intake_id": intake_id, "dry_run": true, "yes": true}),
            )
            .await?;
            assert!(
                resp["error"].is_null(),
                "{method} must still plan a real intake: {resp}"
            );
        }

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

/// The two drift legs of the execute step. They are two independent
/// pieces of code — the intake is gone, and the intake is there but
/// its state moved — so each is pinned on its own; one assertion
/// would leave the other free to regress.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_that_moved_since_the_dry_run_is_reported_as_drift() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let catalog_db = data_root.path().join("catalog.db");
    let vanishing = seed_intake(&catalog_db, ItemKind::Book, "sha-vanishing")?;
    let moving = seed_intake(&catalog_db, ItemKind::Book, "sha-moving")?;
    let papers_catalog_db = data_root.path().join("papers_catalog.db");
    let paper_vanishing = seed_intake(&papers_catalog_db, ItemKind::Paper, "sha-paper-vanishing")?;
    let paper_moving = seed_intake(&papers_catalog_db, ItemKind::Paper, "sha-paper-moving")?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let cases = [
            (
                "remove",
                catalog_db.clone(),
                ItemKind::Book,
                vanishing,
                moving,
            ),
            (
                "papers.remove",
                papers_catalog_db.clone(),
                ItemKind::Paper,
                paper_vanishing,
                paper_moving,
            ),
        ];
        let mut id = 1;
        for (method, db, kind, vanishing, moving) in cases {
            // Leg one: the intake the plan pinned is deleted between
            // the two RPCs, so the execute leg cannot resolve it.
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"intake_id": vanishing, "dry_run": true, "yes": true}),
            )
            .await?;
            let pinned = plan_id(&resp)?;
            {
                let catalog = Catalog::open(&db)?;
                assert!(catalog.delete_intake(vanishing)?, "{method}: seed removed");
            }
            id += 1;
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"plan_id": pinned, "yes": true}),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(PLAN_TARGET_DRIFTED),
                "{method}: the pinned intake vanished: {resp}"
            );

            // Leg two: the intake is still there, but the state the
            // operator confirmed is not.
            id += 1;
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"intake_id": moving, "dry_run": true, "yes": true}),
            )
            .await?;
            let pinned = plan_id(&resp)?;
            {
                let catalog = Catalog::open(&db)?;
                assert!(
                    catalog.set_stored_path(kind, moving, "/nowhere/envelope.json")?,
                    "{method}: seed moved"
                );
            }
            id += 1;
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"plan_id": pinned, "yes": true}),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(PLAN_TARGET_DRIFTED),
                "{method}: the pinned state moved: {resp}"
            );
            let detail = resp["error"]["data"]["detail"].as_str().unwrap_or_default();
            assert!(
                detail.contains("fingerprint"),
                "{method}: the evidence for a refused delete belongs in detail: {resp}"
            );
            id += 1;
        }

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

/// The index writes on a library that has never been ingested into.
/// Nothing is wrong with the request; the library simply has no layer
/// for it to act on, and the operator's next step is to put one there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn index_writes_on_a_library_with_no_chunks_are_caller_input() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        // `drop` is behind the confirmation gate and `rebuild` is not;
        // an unconfirmed drop would be refused at the gate instead,
        // which is a different code and a different leg.
        let cases = [
            (1, "vectors.rebuild", json!({})),
            (2, "vectors.drop", json!({"yes": true})),
            (3, "papers.vectors_rebuild", json!({})),
            (4, "papers.vectors_drop", json!({"yes": true})),
        ];
        for (id, method, params) in cases {
            let resp = call(&mut w, &mut reader, id, method, params).await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_PARAMS),
                "{method} on a library with no chunks: {resp}"
            );
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("chunks"),
                "{method} must name the layer the library is missing: {resp}"
            );
            let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
            assert!(
                !hint.is_empty(),
                "{method} must say what would give the library chunks: {resp}"
            );
        }

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

/// A `kind` outside the accepted set. The fixture carries a
/// `vector_dim` stamp on both corpora, because the stamp check runs
/// first: without it the refusal under test is never reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unsupported_ann_kind_is_refused_with_the_accepted_set() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    for name in ["corpus.db", "papers_corpus.db"] {
        let corpus = bookrack_corpus::Corpus::open(&data_root.path().join(name))?;
        corpus.meta_set(bookrack_corpus::VECTOR_DIM_KEY, "8")?;
    }
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        for (id, method) in [(1, "vectors.rebuild"), (2, "papers.vectors_rebuild")] {
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"kind": "not-an-index-kind"}),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_PARAMS),
                "{method} unsupported kind: {resp}"
            );
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("not-an-index-kind"),
                "{method} must quote the value it refused: {resp}"
            );
            // The whole accepted set, written out here rather than read
            // from `AnnKind::ALL`: transcribing it is what makes this
            // test fail when a new kind stops reaching the message.
            let detail = resp["error"]["data"]["detail"].as_str().unwrap_or_default();
            for kind in [
                "ivf-flat",
                "ivf-sq",
                "ivf-pq",
                "ivf-hnsw-flat",
                "ivf-hnsw-sq",
                "ivf-hnsw-pq",
                "brute-force",
            ] {
                assert!(
                    detail.contains(kind),
                    "{method} must offer {kind} as an accepted value: {resp}"
                );
            }
        }

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

/// A dry run over a directory holding nothing the pipeline can read.
/// The path resolved and the scan ran; there was simply nothing to
/// report on, which is the caller's to fix and not a fault.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dry_run_over_a_directory_with_nothing_to_scan_is_caller_input() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let empty = tempfile::tempdir()?;
    let empty_path = empty.path().to_path_buf();
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        for (id, method) in [(1, "dryrun"), (2, "papers.dryrun")] {
            let resp = call(
                &mut w,
                &mut reader,
                id,
                method,
                json!({"path": empty_path.clone()}),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_PARAMS),
                "{method} over an empty directory: {resp}"
            );
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains(&empty_path.display().to_string()),
                "{method} must name the directory it scanned: {resp}"
            );
            let hint = resp["error"]["data"]["hint"].as_str().unwrap_or_default();
            assert!(
                !hint.is_empty(),
                "{method} must say what it would have accepted: {resp}"
            );
        }

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
