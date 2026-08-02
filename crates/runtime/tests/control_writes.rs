// SPDX-License-Identifier: Apache-2.0

//! Phase 2 control-plane write-surface integration test.
//!
//! Boots a [`DaemonRuntime`] in the headless profile, drives one
//! client through the `ingest.submit` → `queue.tick` event path while
//! a second client observes the broadcast over `events.subscribe`,
//! then exercises the `-32001 busy` error code by holding the
//! runtime's write mutex across a `vectors.drop` and releasing it.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use std::time::Duration;

use eyre::{Context, Result};
use serde_json::Value;

use crate::common::{await_channel, build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_submit_broadcasts_queue_tick_to_subscribers() -> Result<()> {
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
        let (mut obs_reader, mut obs_w) = connect(&sock).await?;
        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":1,"method":"events.subscribe"}"#,
        )
        .await?;
        let resp = recv(&mut obs_reader).await?;
        assert_eq!(resp["result"]["subscribed"], Value::Bool(true), "{resp}");
        // Drain the snapshot bundle so the queue.tick awaited below is
        // the submit broadcast, not the pre-submit snapshot.
        for _ in 0..7 {
            let _ = recv(&mut obs_reader).await?;
        }

        let (mut wr_reader, mut wr_w) = connect(&sock).await?;
        // Pause the worker so the submitted job stays pending: tick
        // values are derived from live queue state at emission time,
        // and a running worker could fail the missing-fixture job
        // before the submit tick is built.
        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":2,"method":"queue.pause"}"#,
        )
        .await?;
        let pause = recv(&mut wr_reader).await?;
        assert_eq!(pause["result"]["paused"], Value::Bool(true), "{pause}");

        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":3,"method":"ingest.submit","params":{"paths":["/tmp/phase2-fixture.txt"]}}"#,
        )
        .await?;
        let submit = recv(&mut wr_reader).await?;
        assert!(submit["result"]["job_ids"].is_array(), "{submit}");

        // Tick values are derived from live queue state at emission
        // time and idle ticks may interleave, so await the first tick
        // that reflects the submitted job rather than a fixed ordinal.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let tick = await_channel(&mut obs_reader, "queue.tick", Duration::from_secs(5))
                .await
                .context("expect queue.tick on observer")?;
            if tick["params"]["value"]["pending"].as_u64().unwrap_or(0) >= 1 {
                break;
            }
            eyre::ensure!(
                tokio::time::Instant::now() < deadline,
                "no queue.tick with pending >= 1 arrived: last {tick}"
            );
        }

        send(
            &mut obs_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut obs_reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_is_refused_while_another_holds_the_write_mutex() -> Result<()> {
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
    // Stand in for a write already in flight by holding the very mutex
    // `run_write` acquires. Racing two RPC writes against each other
    // cannot guarantee they overlap; holding the lock makes the overlap
    // a fact of the test rather than a matter of scheduling.
    let held = runtime.write_guard.clone().lock_owned().await;
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut writer) = connect(&sock).await?;

        // `vectors.drop` is the simplest write command in the surface.
        // With the mutex held it must come back refused — and it must
        // come back at all: the contract is to refuse a concurrent
        // writer, not to queue it behind the holder.
        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":10,"method":"vectors.drop","params":{"yes":true}}"#,
        )
        .await?;
        let refused = recv(&mut reader).await?;
        assert_eq!(
            refused["error"]["code"].as_i64(),
            Some(-32001_i64),
            "a write while the mutex is held must be refused as busy: {refused}"
        );

        // Releasing the mutex lets the same call reach the handler
        // body, so the refusal above is about the mutex rather than
        // about the command. On a library with no chunks the body then
        // refuses on its own terms — a different code, raised from
        // inside `run_write` rather than at its gate.
        drop(held);
        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":11,"method":"vectors.drop","params":{"yes":true}}"#,
        )
        .await?;
        let admitted = recv(&mut reader).await?;
        assert_ne!(
            admitted["error"]["code"].as_i64(),
            Some(-32001_i64),
            "releasing the mutex must stop the refusal: {admitted}"
        );
        assert!(
            admitted["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("no ingested chunks")),
            "the write reached the handler body: {admitted}"
        );

        send(
            &mut writer,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}

/// An `audit_profile` naming no built-in used to fall through to the
/// overlay path: the method ran to completion under a different
/// profile and reported success, so a caller that asked for `strict`
/// silently got the overlay default. Every entry point that accepts
/// the parameter now refuses the name instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_audit_profile_is_refused_at_every_entry_point() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let scan_dir = tempfile::tempdir()?;
    let ocr_md = scan_dir.path().join("x.md");
    let from_pdf = scan_dir.path().join("x.pdf");
    std::fs::write(&ocr_md, "# x\n")?;
    std::fs::write(&from_pdf, "%PDF-1.4\n")?;
    // The positive control runs a real dryrun, so it is pointed at an
    // empty directory: the handler refuses it for having nothing to
    // scan, which is the proof the profile name was accepted.
    let empty_dir = tempfile::tempdir()?;
    // The queue worker must be up: the queue-bound gate answers
    // `-32002` ahead of every handler in headless mode, and would mask
    // what this test is asserting.
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let scan_path = scan_dir.path().to_path_buf();
    let empty_path = empty_dir.path().to_path_buf();

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        // The six entry points that accept `audit_profile`. Each is
        // given otherwise-valid params, so a refusal can only come
        // from the profile name.
        let cases: Vec<(&str, Value)> = vec![
            (
                "dryrun",
                serde_json::json!({"path": scan_path, "audit_profile": "strictt"}),
            ),
            (
                "ingest.submit",
                serde_json::json!({"paths": [scan_path], "audit_profile": "strictt"}),
            ),
            (
                "intake.ocr",
                serde_json::json!({
                    "ocr_md": ocr_md,
                    "from_pdf": from_pdf,
                    "audit_profile": "strictt",
                }),
            ),
            (
                "metadata.reaudit",
                serde_json::json!({"book": 1, "audit_profile": "strictt"}),
            ),
            (
                "metadata.advance",
                serde_json::json!({"book": 1, "audit_profile": "strictt"}),
            ),
            (
                "papers.metadata.reaudit",
                serde_json::json!({"intake_id": 1, "audit_profile": "strictt"}),
            ),
        ];

        for (id, (method, params)) in cases.into_iter().enumerate() {
            let req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id + 1,
                "method": method,
                "params": params,
            });
            send(&mut w, &serde_json::to_string(&req)?).await?;
            let resp = recv(&mut reader).await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(-32602),
                "{method} must refuse an unknown audit_profile: {resp}"
            );
            // The code alone does not discriminate here: four of these
            // six answer `-32602` anyway, because book / intake `1`
            // does not exist either. Quoting the rejected name is what
            // proves the profile guard is the leg that ran. Do not
            // weaken this to the code check.
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("strictt"),
                "{method} must quote the name it refused: {resp}"
            );
            // Without the accepted set the operator learns only that
            // something was wrong, not what to send instead — which is
            // the half of this defect the error code alone leaves open.
            let detail = resp["error"]["data"]["detail"].as_str().unwrap_or_default();
            for name in ["default", "trust-source", "strict"] {
                assert!(
                    detail.contains(name),
                    "{method} must offer the accepted set: {resp}"
                );
            }
        }

        // Positive control: a real built-in still resolves. Without
        // this, a guard that refused every name would pass everything
        // above. The handler then refuses the run on its own terms —
        // the directory holds nothing to scan — which shares the code
        // with the refusals above, so what separates them is that this
        // one does not quote a profile name.
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "dryrun",
            "params": {"path": empty_path, "audit_profile": "strict"},
        });
        send(&mut w, &serde_json::to_string(&req)?).await?;
        let resp = recv(&mut reader).await?;
        let message = resp["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("no supported files"),
            "a built-in profile name must reach the handler body: {resp}"
        );
        assert!(
            !message.contains("strict"),
            "a built-in profile name must not be refused: {resp}"
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
