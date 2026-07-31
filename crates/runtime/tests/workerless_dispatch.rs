// SPDX-License-Identifier: Apache-2.0

//! Dispatch-level contract of a daemon running without a queue
//! worker: queue-bound write methods short-circuit to `-32002
//! not_ready` before their handler runs, while reads and non-queue
//! writes still dispatch normally.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use eyre::Result;
use serde_json::json;

use crate::common::{build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

const QUEUE_WORKER_DISABLED: i64 = -32002;
const INVALID_LIBRARY: i64 = -32010;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_bound_writes_short_circuit_without_a_worker() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        false,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        // Queue-bound writes are refused before the handler runs. The
        // deliberately-empty params would fail parameter validation
        // inside the handler, so getting -32002 instead of
        // INVALID_PARAMS proves the short-circuit fires first.
        for (id, method) in [(1, "ingest.submit"), (2, "glean.submit")] {
            let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}});
            send(&mut w, &req.to_string()).await?;
            let resp = recv(&mut reader).await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(QUEUE_WORKER_DISABLED),
                "{method} must short-circuit to -32002: {resp}"
            );
            assert!(
                resp["error"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("queue worker")),
                "the refusal must name the missing queue worker: {resp}"
            );
        }

        // Reads still serve.
        send(&mut w, r#"{"jsonrpc":"2.0","id":10,"method":"queue.list"}"#).await?;
        let resp = recv(&mut reader).await?;
        assert!(
            resp["result"]["jobs"].is_array(),
            "queue.list must still serve reads: {resp}"
        );

        // A non-queue write still reaches its handler: the unknown
        // library surfaces the handler's own -32010, not -32002.
        let req = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "library.set_default",
            "params": {"name": "no-such-library"},
        });
        send(&mut w, &req.to_string()).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(INVALID_LIBRARY),
            "non-queue writes must dispatch normally: {resp}"
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
