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

use eyre::{Result, eyre};
use serde_json::Value;

use crate::common::{Reader, Writer};
use crate::common::{build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

const INVALID_PARAMS: i64 = -32602;
const INVALID_LIBRARY: i64 = -32010;
const CONFIRMATION_REQUIRED: i64 = -32012;

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
