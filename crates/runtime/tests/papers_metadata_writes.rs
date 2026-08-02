// SPDX-License-Identifier: Apache-2.0

//! Control-plane integration test for the paper-side metadata write
//! surface: every method that addresses an intake must refuse an id
//! the paper catalog does not hold, and must refuse it *before*
//! writing.
//!
//! The paper override, review, and contributor tables carry no foreign
//! key onto `intakes`, so a write against a phantom id used to succeed
//! and leave a row nothing reads and `remove` never cascades away. The
//! error code alone does not pin that: the row check does.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use bookrack_catalog::Catalog;
use bookrack_core::ItemKind;
use eyre::{Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

/// An id no intake in an empty library can carry.
const PHANTOM: i64 = 999_999;

/// Issue one request and return the whole response frame.
async fn call(
    reader: &mut Reader,
    w: &mut Writer,
    id: i64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    send(w, &serde_json::to_string(&req)?).await?;
    recv(reader).await
}

/// Assert one call was refused as caller input and named the id it
/// refused. Naming the id is what lets an operator tell "I typed the
/// wrong number" from "the server is broken".
fn assert_unknown_intake(resp: &Value, method: &str) {
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "{method} must refuse a phantom intake as caller input: {resp}"
    );
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("999999"),
        "{method} must name the id it refused: {resp}"
    );
    assert!(
        resp["result"].is_null(),
        "{method} must not also report success: {resp}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paper_metadata_writes_refuse_an_intake_the_catalog_does_not_hold() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    // The daemon owns this path; the assertion below reopens it after
    // shutdown rather than racing the daemon's own handle.
    let papers_catalog = data_root.path().join("papers_catalog.db");
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

        // 1. `set` against a phantom id is caller input, not a fault.
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "papers.metadata.set",
            json!({"intake_id": PHANTOM, "field": "title", "value": "x"}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.set");

        // 2. The soft-report pair reports the refusal too, rather than
        //    `removed: false` / a success envelope.
        let resp = call(
            &mut reader,
            &mut w,
            2,
            "papers.metadata.clear",
            json!({"intake_id": PHANTOM, "field": "title"}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.clear");

        let resp = call(
            &mut reader,
            &mut w,
            3,
            "papers.metadata.void",
            json!({"intake_id": PHANTOM, "field": "title"}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.void");

        // 3. The four review verbs share one write path; each is
        //    dispatched separately, so each is asserted separately.
        for (id, method) in [
            (4, "papers.metadata.ack"),
            (5, "papers.metadata.approve"),
            (6, "papers.metadata.reject"),
            (7, "papers.metadata.reopen"),
        ] {
            let resp = call(
                &mut reader,
                &mut w,
                id,
                method,
                json!({"intake_id": PHANTOM}),
            )
            .await?;
            assert_unknown_intake(&resp, method);
        }

        // 4. `contributor_add` writes to a third table with the same
        //    missing foreign key.
        let resp = call(
            &mut reader,
            &mut w,
            8,
            "papers.metadata.contributor_add",
            json!({"intake_id": PHANTOM, "role": "author", "name": "x"}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.contributor_add");

        // 5. Regression guard: `reaudit` already reached -32602 through
        //    the typed glean error, and must keep doing so — the local
        //    guard is deliberately not on that path.
        let resp = call(
            &mut reader,
            &mut w,
            9,
            "papers.metadata.reaudit",
            json!({"intake_id": PHANTOM}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.reaudit");

        // 6. Regression guard: the book side answers the same input the
        //    same way. The two surfaces agreeing is the property that
        //    made this gap worth closing.
        let resp = call(
            &mut reader,
            &mut w,
            10,
            "metadata.set",
            json!({"book": PHANTOM, "field": "title", "value": "x"}),
        )
        .await?;
        assert_unknown_intake(&resp, "metadata.set");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    // 7. The assertion the codes cannot make: nothing was written. A
    //    guard that returns -32602 and then falls through to the write
    //    would satisfy every check above and none of these.
    let catalog = Catalog::open(&papers_catalog)
        .map_err(|e| eyre!("reopen paper catalog at {}: {e}", papers_catalog.display()))?;
    assert!(
        catalog
            .overrides_for_address(PHANTOM, ItemKind::Paper)?
            .is_empty(),
        "a refused set/void must leave no override row behind"
    );
    assert!(
        catalog
            .contributors_for_address(PHANTOM, ItemKind::Paper)?
            .is_empty(),
        "a refused contributor_add must leave no contributor row behind"
    );
    assert!(
        catalog.review(PHANTOM, ItemKind::Paper)?.is_none(),
        "a refused review verb must leave no review row behind"
    );
    Ok(())
}
