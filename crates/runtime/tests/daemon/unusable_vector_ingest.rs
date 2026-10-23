// SPDX-License-Identifier: Apache-2.0

//! An embedding backend that returns a vector no cosine index can use
//! fails the ingest job instead of storing it.
//!
//! The failure is the backend's: the job ends `failed`, its error text
//! carries the embed error's own sentence, and the item is not marked
//! embedded. The refusal is per batch, not per book: a book's pipeline
//! clears the book's old rows and then writes batch by batch, so the
//! batches written before the bad one stay. The paper pipeline embeds
//! a paper's chunks in one batch, so nothing of it lands.
//!
//! Lives in its own binary because `EmbedStub::set_failure` is
//! process-global.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_test_support::{EmbedFailure, EmbedStub, ProcessEnv, process_env};
use bookrack_vectors::ChunkStore;
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

/// One pipeline's submit method and stores.
#[derive(Clone, Copy)]
struct Side {
    submit: &'static str,
    catalog: &'static str,
    lancedb: &'static str,
}

const BOOKS: Side = Side {
    submit: "ingest.submit",
    catalog: "catalog.db",
    lancedb: "lancedb",
};

const PAPERS: Side = Side {
    submit: "glean.submit",
    catalog: "papers_catalog.db",
    lancedb: "lancedb_papers",
};

/// The sentence the embed error's `Display` opens with.
const UNUSABLE: &str = "Ollama returned an unusable vector at position";

/// A synthetic text of `paragraphs` paragraphs, each distinct, with the
/// stub's marker in every paragraph from `marked_from` on.
fn synthetic_text(paragraphs: usize, marked_from: usize) -> String {
    let mut text = String::from("Chapter One\n\n");
    for i in 0..paragraphs {
        let marker = if i >= marked_from {
            EmbedStub::UNUSABLE_MARKER
        } else {
            "unmarked"
        };
        text.push_str(&format!(
            "Paragraph {i} of the synthetic survey records a fictional river and its \
             fictional banks. The surveyor measured the river at station {i}, noted the \
             width and the depth, and filed the note as {marker}. Every station along the \
             fictional river had a fictional ferry, and every ferry had a fictional keeper \
             who logged the crossings of the day in a ledger numbered {i}.\n\n"
        ));
    }
    text
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

/// Submit `path` through `side` and return the queue row its job ends
/// in.
async fn run_to_end(reader: &mut Reader, w: &mut Writer, side: Side, path: &Path) -> Result<Value> {
    let resp = call(reader, w, 1, side.submit, json!({ "paths": [path] })).await?;
    let job = resp["result"]["job_ids"][0]
        .as_str()
        .with_context(|| format!("no job id: {resp}"))?
        .to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let listed = call(reader, w, 2, "queue.list", Value::Null).await?;
        let row = listed["result"]["jobs"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(&job)))
            .cloned()
            .with_context(|| format!("job {job} is not listed: {listed}"))?;
        match row["state"].as_str() {
            Some("pending" | "running") if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Some("pending" | "running") => return Err(eyre!("the job is still open: {row}")),
            _ => return Ok(row),
        }
    }
}

async fn chunk_rows(root: &Path, side: Side) -> Result<usize> {
    Ok(
        match ChunkStore::try_open(&root.join(side.lancedb)).await? {
            Some(store) => store.count_rows().await?,
            None => 0,
        },
    )
}

fn embedded(root: &Path, side: Side) -> Result<usize> {
    Ok(Catalog::open_read_only(&root.join(side.catalog))?
        .intakes_with_status(IntakeStatus::Embedded)?
        .len())
}

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

/// The job failed on the embed error's own sentence, and the item is
/// not embedded.
fn assert_failed_on_the_vector(side: Side, row: &Value, root: &Path) -> Result<()> {
    assert_eq!(
        row["state"].as_str(),
        Some("failed"),
        "{}: {row}",
        side.submit
    );
    let error = row["error"].as_str().unwrap_or_default();
    assert!(
        error.starts_with("ingest:") && error.contains(UNUSABLE),
        "{}: the failure names the unusable vector: {row}",
        side.submit
    );
    assert_eq!(
        embedded(root, side)?,
        0,
        "{}: a failed item is not embedded",
        side.submit
    );
    Ok(())
}

/// The marker sits only in the book's last paragraphs, past what the
/// first embed batch holds, so the first batch is written and a later
/// one is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_book_batch_with_an_unusable_vector_fails_the_job_and_keeps_earlier_batches() -> Result<()>
{
    with_daemon(|sock, root| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let sources = tempfile::tempdir()?;
        let book = sources.path().join("synthetic-survey.txt");
        std::fs::write(&book, synthetic_text(40, 32))?;

        EmbedStub::set_failure(EmbedFailure::UnusableVector);
        let row = run_to_end(&mut r, &mut w, BOOKS, &book).await?;
        assert_failed_on_the_vector(BOOKS, &row, &root)?;
        assert!(
            chunk_rows(&root, BOOKS).await? > 0,
            "the batches before the refused one stay: this is not a transaction"
        );
        Ok(())
    })
    .await
}

/// Every paragraph carries the marker, so whichever text the paper
/// pipeline chunks is refused, and its single batch writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_with_an_unusable_vector_fails_the_job_and_writes_nothing() -> Result<()> {
    with_daemon(|sock, root| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let sources = tempfile::tempdir()?;
        let paper = sources.path().join("synthetic-paper.txt");
        std::fs::write(&paper, synthetic_text(3, 0))?;

        EmbedStub::set_failure(EmbedFailure::UnusableVector);
        let row = run_to_end(&mut r, &mut w, PAPERS, &paper).await?;
        assert_failed_on_the_vector(PAPERS, &row, &root)?;
        assert_eq!(chunk_rows(&root, PAPERS).await?, 0);
        Ok(())
    })
    .await
}
