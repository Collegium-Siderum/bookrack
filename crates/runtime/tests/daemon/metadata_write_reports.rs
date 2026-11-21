// SPDX-License-Identifier: Apache-2.0

//! The book-side metadata writes answer with what the ops layer
//! recorded: the audit row an edit wrote and the verdict a re-audit
//! stored. Each claim is read back from the store it describes. An
//! approval on a book parked at the metadata gate records the review
//! and leaves the book parked; `metadata.advance` is not a method.
//!
//! A bare text with no bibliographic data audits as `needs_work`, so
//! `ingest.submit { hold_for_metadata: true }` parks it at the gate
//! through the real worker. The embedder is
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_test_support::{ProcessEnv, process_env};
use bookrack_vectors::ChunkReader;
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
    Ok(Catalog::open_for_write_unbacked(&root.join("catalog.db"))?
        .book_state(PartitionIdx::new(book).root().get())?
        .map(|state| state.current_stage))
}

/// Whether a reply's `audit_id` names a row of the `metadata_audit`
/// table.
fn audit_row_exists(root: &Path, reply: &Value) -> Result<bool> {
    let audit_id = reply["audit_id"]
        .as_i64()
        .with_context(|| format!("no audit_id: {reply}"))?;
    Ok(Catalog::open_for_write_unbacked(&root.join("catalog.db"))?
        .recent_metadata_audit("", 1000)?
        .iter()
        .any(|row| row.audit_id == audit_id))
}

/// The chunk rows the vector store holds for one book; none when the
/// chunks table does not exist yet.
async fn chunk_rows_of(root: &Path, book: i64) -> Result<usize> {
    let Some(reader) = ChunkReader::open(&root.join("lancedb")).await? else {
        return Ok(0);
    };
    Ok(reader.scan_partition(PartitionIdx::new(book)).await?.len())
}

/// Submit `params` to `ingest.submit`, wait until the worker has
/// finished the one job it queued, and return the one intake the
/// library then holds.
async fn ingest_and_wait(
    reader: &mut Reader,
    w: &mut Writer,
    root: &Path,
    params: Value,
) -> Result<i64> {
    let submitted = ok_result(reader, w, 1, "ingest.submit", params).await?;
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
    let intakes = Catalog::open_for_write_unbacked(&root.join("catalog.db"))?.intakes_head(10)?;
    let [intake] = intakes.as_slice() else {
        return Err(eyre!("expected one intake, found {}", intakes.len()));
    };
    Ok(intake.intake_id)
}

/// Write a bare text with no bibliographic data under `dir`.
fn write_bare(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("bare-sample.txt");
    std::fs::write(
        &path,
        "The first paragraph of a short synthetic document about birds.\n\
         A second paragraph carrying more prose to chunk and embed.\n",
    )?;
    Ok(path)
}

/// Ingest a bare text with the metadata hold on, wait until the worker
/// has parked it at the gate, and return its intake id.
async fn park_one(reader: &mut Reader, w: &mut Writer, root: &Path, dir: &Path) -> Result<i64> {
    let path = write_bare(dir)?;
    let params = json!({ "paths": [path], "hold_for_metadata": true });
    let book = ingest_and_wait(reader, w, root, params).await?;
    assert_eq!(
        stage_of(root, book)?.as_deref(),
        Some("metadata"),
        "a bare text must park at the metadata gate"
    );
    Ok(book)
}

/// Ingest a bare text through the real worker, without the hold, and
/// return its intake id.
async fn ingest_one(reader: &mut Reader, w: &mut Writer, root: &Path, dir: &Path) -> Result<i64> {
    let path = write_bare(dir)?;
    ingest_and_wait(reader, w, root, json!({ "paths": [path] })).await
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

/// An approval on a book that is not parked records the review and
/// answers with the edit's fields.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approve_off_the_gate_reports_the_edit() -> Result<()> {
    with_daemon(|sock, root| async move {
        let book = Catalog::open_for_write_unbacked(&root.join("catalog.db"))?
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
                "ok"
            ],
            "{reply}"
        );
        assert!(audit_row_exists(&root, &reply)?, "{reply}");
        Ok(())
    })
    .await
}

/// A re-audit reports the verdict it stored, beside the one it
/// replaced, and writes no audit row of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reaudit_reports_the_verdict_it_stored() -> Result<()> {
    with_daemon(|sock, root| async move {
        let sources = tempfile::tempdir()?;
        let (mut r, mut w) = connect(&sock).await?;
        let book = ingest_one(&mut r, &mut w, &root, sources.path()).await?;

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
        Ok(())
    })
    .await
}

/// An approval on a book parked at the metadata gate only records the
/// review: the reply is the edit's, the book stays at the gate, and no
/// vectors are written for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approve_at_the_gate_leaves_the_book_parked() -> Result<()> {
    with_daemon(|sock, root| async move {
        let sources = tempfile::tempdir()?;
        let (mut r, mut w) = connect(&sock).await?;
        let book = park_one(&mut r, &mut w, &root, sources.path()).await?;

        let approved = ok_result(
            &mut r,
            &mut w,
            4,
            "metadata.approve",
            json!({ "book": book }),
        )
        .await?;
        assert_eq!(
            keys(&approved),
            [
                "actor_detail",
                "actor_kind",
                "audit_id",
                "book",
                "changed",
                "library",
                "ok"
            ],
            "{approved}"
        );
        assert!(audit_row_exists(&root, &approved)?, "{approved}");
        assert_eq!(
            stage_of(&root, book)?.as_deref(),
            Some("metadata"),
            "an approval moved the book off the gate"
        );
        assert_eq!(
            chunk_rows_of(&root, book).await?,
            0,
            "an approval wrote vectors for the parked book"
        );
        Ok(())
    })
    .await
}

/// `metadata.advance` is not a method.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_advance_is_not_a_method() -> Result<()> {
    with_daemon(|sock, _root| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let resp = call(&mut r, &mut w, 1, "metadata.advance", json!({ "book": 1 })).await?;
        assert_eq!(resp["error"]["code"].as_i64(), Some(-32601), "{resp}");
        Ok(())
    })
    .await
}
