// SPDX-License-Identifier: Apache-2.0

//! Control-plane integration test for the paper-side maintenance
//! triplet: `papers.corpus_rebuild`, `papers.vectors_*`, and
//! `papers.stamps_reconcile`.
//!
//! Drives the daemon's JSON-RPC dispatch, asserts the maintenance
//! methods appear under `daemon.methods`, and exercises the dry-run
//! paths that do not require a populated library to validate the
//! parameter shapes and the queue-bound write gate.
//!
//! The embedder probe daemon bring-up performs is answered by the
//! loopback stub in `common`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::collections::BTreeSet;

use eyre::{Result, eyre};
use serde_json::json;

use crate::common::{build_opts, connect, init_test_env, join_with_deadline, recv, send};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_maintenance_methods_are_dispatched_and_callable_on_empty_library() -> Result<()> {
    init_test_env();
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

        // 1. `daemon.methods` enumerates every maintenance method.
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"daemon.methods"}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let names: BTreeSet<String> = resp["result"]["methods"]
            .as_array()
            .ok_or_else(|| eyre!("daemon.methods missing array: {resp}"))?
            .iter()
            .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
            .collect();
        for expected in [
            "papers.corpus_rebuild",
            "papers.vectors_rebuild",
            "papers.vectors_reembed",
            "papers.vectors_reset",
            "papers.vectors_drop",
            "papers.stamps_reconcile",
            "papers.dryrun",
        ] {
            assert!(
                names.contains(expected),
                "method {expected} missing from daemon.methods: {names:?}"
            );
        }

        // 2. `papers.corpus_rebuild` with dry_run=true on an empty
        //    library registers a plan and reports zero rebuildable
        //    intakes across every bucket.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "papers.corpus_rebuild",
            "params": {"dry_run": true, "yes": true},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        let result = &resp["result"];
        assert!(
            result["plan_id"].as_str().is_some_and(|id| !id.is_empty()),
            "papers.corpus_rebuild dry_run must return a plan_id: {resp}"
        );
        for bucket in ["rebuilt", "missing_envelope", "mismatched", "failed"] {
            assert_eq!(
                result[bucket],
                json!([]),
                "empty library must report an empty `{bucket}` bucket: {resp}"
            );
        }

        // 3. The dry-run leg is exempt from the yes gate: confirmation
        //    protects the execute leg; dry_run only reads and pins.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "papers.corpus_rebuild",
            "params": {"dry_run": true},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        let exec_plan = resp["result"]["plan_id"]
            .as_str()
            .ok_or_else(|| eyre!("dry_run without yes must still pin a plan: {resp}"))?
            .to_string();

        // 4. The execute leg without a plan_id is refused: the
        //    two-phase protocol requires the dry-run's pinned plan.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "papers.corpus_rebuild",
            "params": {"yes": true},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["error"]["code"].as_i64(), Some(-32602), "{resp}");
        assert!(
            resp["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("plan_id")),
            "the refusal must name the missing plan_id: {resp}"
        );

        // 5. The yes gate runs ahead of the plan lookup: an
        //    unconfirmed execute fails CONFIRMATION_REQUIRED even with
        //    a bogus plan id.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "papers.corpus_rebuild",
            "params": {"plan_id": "bogus-plan"},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["error"]["code"].as_i64(), Some(-32012), "{resp}");

        // 6. A confirmed execute with an unknown plan id is
        //    PLAN_NOT_FOUND.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "papers.corpus_rebuild",
            "params": {"yes": true, "plan_id": "bogus-plan"},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(resp["error"]["code"].as_i64(), Some(-32013), "{resp}");

        // 7. The pinned plan executes once against the empty library…
        let req = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "papers.corpus_rebuild",
            "params": {"yes": true, "plan_id": exec_plan.as_str()},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(
            resp["result"]["rebuilt"],
            json!([]),
            "executing an empty pinned plan reports an empty rebuilt bucket: {resp}"
        );

        // 8. …and is consumed by that execute: a replay of the same
        //    plan id is PLAN_NOT_FOUND, never a second run.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "papers.corpus_rebuild",
            "params": {"yes": true, "plan_id": exec_plan.as_str()},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32013),
            "a consumed plan must not execute twice: {resp}"
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
