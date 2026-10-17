// SPDX-License-Identifier: Apache-2.0

//! The book-side metadata writes answer with what the ops layer
//! recorded: the audit row an edit wrote, the verdict a re-audit
//! stored, and — for `approve` on a book parked at the metadata gate,
//! and for `advance` — what the resumed CHUNK→EMBED wrote. Each claim is
//! read back from the store it describes.
//!
//! A bare text with no bibliographic data audits as `needs_work`, so
//! `ingest.submit { hold_for_metadata: true }` parks it at the gate
//! through the real worker. The embedder is
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_test_support::{ProcessEnv, process_env};
use bookrack_vectors::ChunkStore;
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

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
    if !resp["error"].is_null() {
        return Err(eyre!("{method} failed: {resp}"));
    }
    Ok(resp["result"].clone())
}

/// The keys of a reply, sorted.
fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

/// The book's current pipeline stage.
fn stage_of(root: &Path, book: i64) -> Result<Option<String>> {
    Ok(Catalog::open(&root.join("catalog.db"))?
        .book_state(PartitionIdx::new(book).root().get())?
        .map(|state| state.current_stage))
}

/// Whether a reply's `audit_id` names a row of the `metadata_audit`
/// table.
fn audit_row_exists(root: &Path, reply: &Value) -> Result<bool> {
    let audit_id = reply["audit_id"]
        .as_i64()
        .with_context(|| format!("no audit_id: {reply}"))?;
    Ok(Catalog::open(&root.join("catalog.db"))?
        .recent_metadata_audit("", 1000)?
        .iter()
        .any(|row| row.audit_id == audit_id))
}

/// The chunk rows the vector store holds for one book.
async fn chunk_rows_of(root: &Path, book: i64) -> Result<usize> {
    Ok(ChunkStore::try_open(&root.join("lancedb"))
        .await?
        .context("the chunks table exists")?
        .scan_partition(PartitionIdx::new(book))
        .await?
        .len())
}

/// Ingest a bare text with the metadata hold on, wait until the worker
/// has parked it at the gate, and return its intake id.
async fn park_one(reader: &mut Reader, w: &mut Writer, root: &Path, dir: &Path) -> Result<i64> {
    let path = dir.join("bare-sample.txt");
    std::fs::write(
        &path,
        "The first paragraph of a short synthetic document about birds.\n\
         A second paragraph carrying more prose to chunk and embed.\n",
    )?;
    let submitted = ok_result(
        reader,
        w,
        1,
        "ingest.submit",
        json!({ "paths": [path], "hold_for_metadata": true }),
    )
    .await?;
    let job = submitted["job_ids"][0]
        .as_str()
        .with_context(|| format!("no job id: {submitted}"))?
        .to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        let listed = ok_result(reader, w, 2, "queue.list", Value::Null).await?;
        let state = listed["jobs"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(&job)))
            .and_then(|row| row["state"].as_str())
            .unwrap_or("missing")
            .to_string();
        match state.as_str() {
            "done" => break,
            "failed" | "cancelled" | "missing" => {
                return Err(eyre!("the fixture job did not finish clean: {listed}"));
            }
            _ if std::time::Instant::now() > deadline => {
                return Err(eyre!("the fixture job is still {state}"));
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let intakes = Catalog::open(&root.join("catalog.db"))?.intakes_head(10)?;
    let [intake] = intakes.as_slice() else {
        return Err(eyre!("expected one intake, found {}", intakes.len()));
    };
    let book = intake.intake_id;
    assert_eq!(
        stage_of(root, book)?.as_deref(),
        Some("metadata"),
        "a bare text must park at the metadata gate"
    );
    Ok(book)
}

/// Bring a daemon up on a fresh path-selected root, hand `body` the
/// socket and the root, then shut it down.
async fn with_daemon<F, Fut>(body: F) -> Result<()>
where
    F: FnOnce(PathBuf, PathBuf) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
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
    let root = data_root.path().to_path_buf();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let result = body(sock.clone(), root).await;
        let (mut reader, mut w) = connect(&sock).await?;
        let _ = call(
            &mut reader,
            &mut w,
            u64::MAX,
            "daemon.shutdown",
            Value::Null,
        )
        .await?;
        result
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// An approval on a book that is not parked only records the review:
/// `resumed` is false and no resume counts appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approve_off_the_gate_does_not_resume() -> Result<()> {
    with_daemon(|sock, root| async move {
        let book = Catalog::open(&root.join("catalog.db"))?
            .register_intake(
                ItemKind::Book,
                &NewIntake::new("sha-off-gate").format("epub"),
            )?
            .into_intake()
            .intake_id;
        let (mut r, mut w) = connect(&sock).await?;
        let reply = ok_result(
            &mut r,
            &mut w,
            1,
            "metadata.approve",
            json!({ "book": book }),
        )
        .await?;
        assert_eq!(
            keys(&reply),
            [
                "actor_detail",
                "actor_kind",
                "audit_id",
                "book",
                "changed",
                "library",
                "ok",
                "resumed"
            ],
            "{reply}"
        );
        assert_eq!(reply["resumed"], false);
        assert!(audit_row_exists(&root, &reply)?, "{reply}");
        Ok(())
    })
    .await
}

/// A re-audit reports the verdict it stored and writes no audit row of
/// its own; an approval on the parked book records the review, resumes
/// it, and reports the rows the resume wrote — the rows the store holds
/// for that book afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approve_at_the_gate_resumes_and_reports_the_rows_it_wrote() -> Result<()> {
    with_daemon(|sock, root| async move {
        let sources = tempfile::tempdir()?;
        let (mut r, mut w) = connect(&sock).await?;
        let book = park_one(&mut r, &mut w, &root, sources.path()).await?;

        let reaudit = ok_result(
            &mut r,
            &mut w,
            3,
            "metadata.reaudit",
            json!({ "book": book }),
        )
        .await?;
        assert_eq!(
            keys(&reaudit),
            [
                "book",
                "confidence",
                "library",
                "ok",
                "previous_confidence",
                "previous_verdict",
                "verdict"
            ],
            "{reaudit}"
        );
        assert!(reaudit["verdict"].is_string(), "{reaudit}");

        let approved = ok_result(
            &mut r,
            &mut w,
            4,
            "metadata.approve",
            json!({ "book": book }),
        )
        .await?;
        assert_eq!(approved["ok"], true, "{approved}");
        assert_eq!(approved["library"], "default", "{approved}");
        assert_eq!(approved["resumed"], true, "{approved}");
        assert!(audit_row_exists(&root, &approved)?, "{approved}");
        let written = approved["chunks_written"]
            .as_u64()
            .with_context(|| format!("no chunks_written: {approved}"))?
            as usize;
        assert!(written > 0, "{approved}");
        assert_eq!(written, chunk_rows_of(&root, book).await?, "{approved}");
        assert!(
            approved["batches"].as_u64().is_some_and(|n| n > 0),
            "{approved}"
        );
        assert_ne!(stage_of(&root, book)?.as_deref(), Some("metadata"));
        Ok(())
    })
    .await
}

/// An advance reports the rows its resume wrote, which are the rows the
/// store holds for that book afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_advance_reports_the_rows_it_wrote() -> Result<()> {
    with_daemon(|sock, root| async move {
        let sources = tempfile::tempdir()?;
        let (mut r, mut w) = connect(&sock).await?;
        let book = park_one(&mut r, &mut w, &root, sources.path()).await?;

        let reply = ok_result(
            &mut r,
            &mut w,
            3,
            "metadata.advance",
            json!({ "book": book }),
        )
        .await?;
        assert_eq!(
            keys(&reply),
            [
                "batches",
                "book",
                "chunks_written",
                "library",
                "ok",
                "resumed"
            ],
            "{reply}"
        );
        assert_eq!(reply["resumed"], true);
        let written = reply["chunks_written"].as_u64().map(|n| n as usize);
        assert_eq!(written, Some(chunk_rows_of(&root, book).await?), "{reply}");
        assert!(written.is_some_and(|n| n > 0), "{reply}");
        Ok(())
    })
    .await
}
