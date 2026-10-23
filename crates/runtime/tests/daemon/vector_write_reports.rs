// SPDX-License-Identifier: Apache-2.0

//! The vector and stamp writes answer with what they did.
//!
//! `vectors.drop`, `vectors.reset`, and `stamps.reconcile` — and their
//! paper-side peers, which answer in the same shape — return the index
//! kind they left, the counts of what they re-embedded, and the stamps
//! they wrote, beside `ok` and `library`. Each claim is read back from
//! the store it describes rather than from the reply alone.
//!
//! A rebuild the store cannot train is refused as caller input with a
//! next step, and the refusal leaves the index it found and its sidecar
//! as they were.
//!
//! The libraries are filled through the real submit path: a synthetic
//! text goes through `ingest.submit` or `glean.submit`, and the queue
//! worker carries it to `Embedded` against
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_corpus::{
    CHUNK_VERSION_KEY, Corpus, EMBED_MODEL_KEY, NORMALIZE_VERSION_KEY, VECTOR_DIM_KEY,
};
use bookrack_test_support::{EmbedStub, ProcessEnv, process_env};
use bookrack_vectors::{ChunkStore, META_FILENAME};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

/// One pipeline's write surface and the stores it writes to.
#[derive(Clone, Copy)]
struct Side {
    submit: &'static str,
    rebuild: &'static str,
    drop: &'static str,
    reset: &'static str,
    reconcile: &'static str,
    kind: ItemKind,
    catalog: &'static str,
    corpus: &'static str,
    lancedb: &'static str,
    /// The prefix of the pipeline run id a reset audits each item
    /// under.
    reset_audit_prefix: &'static str,
}

const BOOKS: Side = Side {
    submit: "ingest.submit",
    rebuild: "vectors.rebuild",
    drop: "vectors.drop",
    reset: "vectors.reset",
    reconcile: "stamps.reconcile",
    kind: ItemKind::Book,
    catalog: "catalog.db",
    corpus: "corpus.db",
    lancedb: "lancedb",
    reset_audit_prefix: "reset-",
};

const PAPERS: Side = Side {
    submit: "glean.submit",
    rebuild: "papers.vectors_rebuild",
    drop: "papers.vectors_drop",
    reset: "papers.vectors_reset",
    reconcile: "papers.stamps_reconcile",
    kind: ItemKind::Paper,
    catalog: "papers_catalog.db",
    corpus: "papers_corpus.db",
    lancedb: "lancedb_papers",
    reset_audit_prefix: "glean-reset-",
};

/// A synthetic text long enough for either pipeline to extract prose,
/// plan chunks, and embed them against the stub. `tag` makes each
/// copy a distinct source.
fn write_source(dir: &Path, tag: &str) -> Result<PathBuf> {
    let path = dir.join(format!("synthetic-ledger-{tag}.txt"));
    std::fs::write(
        &path,
        format!(
            "Chapter One\n\n\
             The synthetic narrator opened the synthetic ledger and began to \
             count. Every entry in the ledger described a fictional object, and \
             every fictional object had a fictional weight. Counting the weights \
             took the narrator most of the morning.\n\n\
             By noon the ledger was balanced. The narrator celebrated with a \
             fictional meal and recorded the celebration as a footnote.\n\n\
             Chapter Two\n\n\
             On the second day the ledger grew a second column, numbered {tag}. \
             The new column held colours instead of weights, and the narrator \
             sorted them from dull to bright.\n"
        ),
    )?;
    Ok(path)
}

/// One request, the whole response frame back.
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

/// Submit `paths` through `side` and wait until the worker has carried
/// every job to `done`.
async fn fill(
    reader: &mut Reader,
    w: &mut Writer,
    id: &mut u64,
    side: Side,
    paths: &[PathBuf],
) -> Result<()> {
    *id += 1;
    let submitted = ok_result(reader, w, *id, side.submit, json!({ "paths": paths })).await?;
    let jobs: Vec<String> = submitted["job_ids"]
        .as_array()
        .with_context(|| format!("no job ids: {submitted}"))?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        *id += 1;
        let listed = ok_result(reader, w, *id, "queue.list", Value::Null).await?;
        let rows = listed["jobs"]
            .as_array()
            .with_context(|| format!("no jobs array: {listed}"))?;
        let states: Vec<&str> = jobs
            .iter()
            .map(|job| {
                rows.iter()
                    .find(|row| row["id"].as_str() == Some(job))
                    .and_then(|row| row["state"].as_str())
                    .unwrap_or("missing")
            })
            .collect();
        if states.iter().all(|s| *s == "done") {
            return Ok(());
        }
        if states
            .iter()
            .any(|s| matches!(*s, "failed" | "cancelled" | "missing"))
        {
            return Err(eyre!("a fixture job did not finish clean: {listed}"));
        }
        if std::time::Instant::now() > deadline {
            return Err(eyre!("fixture jobs still running: {states:?}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn intakes(root: &Path, side: Side, status: IntakeStatus) -> Result<Vec<i64>> {
    Ok(Catalog::open(&root.join(side.catalog))?
        .intakes_with_status(status)?
        .into_iter()
        .map(|i| i.intake_id)
        .collect())
}

async fn chunk_rows(root: &Path, side: Side) -> Result<usize> {
    Ok(ChunkStore::try_open(&root.join(side.lancedb))
        .await?
        .context("the chunks table exists")?
        .count_rows()
        .await?)
}

async fn chunk_rows_of(root: &Path, side: Side, intake_id: i64) -> Result<usize> {
    Ok(ChunkStore::try_open(&root.join(side.lancedb))
        .await?
        .context("the chunks table exists")?
        .scan_partition(PartitionIdx::new(intake_id))
        .await?
        .len())
}

/// Whether a reset pass audited any step of `intake_id`.
fn reset_audited(root: &Path, side: Side, intake_id: i64) -> Result<bool> {
    let root_id = PartitionIdx::new(intake_id).root().get();
    Ok(Catalog::open(&root.join(side.catalog))?
        .pipeline_audit_for_book(root_id)?
        .iter()
        .any(|row| row.pipeline_run_id.starts_with(side.reset_audit_prefix)))
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

/// On an empty library a drop is refused as before; on a filled one
/// it answers with the kind it left, which is the kind the store's
/// sidecar records.
async fn drop_reports_the_demoted_kind(side: Side, sock: PathBuf, root: PathBuf) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let mut id = 0;
    let sources = tempfile::tempdir()?;

    id += 1;
    let empty = call(&mut r, &mut w, id, side.drop, json!({ "yes": true })).await?;
    assert_eq!(
        empty["error"]["code"].as_i64(),
        Some(-32602),
        "{}: an empty library has no index to drop: {empty}",
        side.drop
    );

    fill(
        &mut r,
        &mut w,
        &mut id,
        side,
        &[write_source(sources.path(), "a")?],
    )
    .await?;

    id += 1;
    let reply = ok_result(&mut r, &mut w, id, side.drop, json!({ "yes": true })).await?;
    assert_eq!(
        reply,
        json!({ "ok": true, "kind": "brute-force", "library": "default" }),
        "{}",
        side.drop
    );
    let sidecar: Value = serde_json::from_slice(&std::fs::read(
        root.join(side.lancedb).join("vectors_meta.json"),
    )?)?;
    assert_eq!(sidecar["kind"], reply["kind"], "{sidecar}");
    Ok(())
}

/// The reply is the stamps the corpus holds afterwards, field for
/// field, at the width the embedder answers with.
async fn reconcile_returns_the_stamps_it_wrote(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let reply = ok_result(&mut r, &mut w, 1, side.reconcile, json!({})).await?;
    assert_eq!(reply["ok"], Value::Bool(true), "{reply}");
    assert_eq!(reply["library"], "default", "{reply}");
    assert_eq!(
        reply["vector_dim"].as_u64(),
        Some(EmbedStub::DIMENSION as u64),
        "{reply}"
    );
    let corpus = Corpus::open(&root.join(side.corpus))?;
    for (field, key) in [
        ("embed_model", EMBED_MODEL_KEY),
        ("vector_dim", VECTOR_DIM_KEY),
        ("chunk_version", CHUNK_VERSION_KEY),
        ("normalize_version", NORMALIZE_VERSION_KEY),
    ] {
        let replied = match &reply[field] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        assert_eq!(
            corpus.meta_get(key)?,
            Some(replied),
            "{}: `{field}` must be the stamp written: {reply}",
            side.reconcile
        );
    }
    Ok(())
}

/// A resume with nothing waiting says so with zeros instead of a bare
/// `ok`.
async fn reset_resume_with_nothing_to_resume_reports_zero(
    side: Side,
    sock: PathBuf,
    _root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let reply = ok_result(&mut r, &mut w, 1, side.reset, json!({ "resume": true })).await?;
    assert_eq!(
        reply,
        json!({
            "ok": true,
            "library": "default",
            "resumed": true,
            "intakes_reembedded": 0,
            "chunks_written": 0,
            "skipped_empty": [],
        }),
        "{}",
        side.reset
    );
    Ok(())
}

/// A fresh reset demotes every embedded item and then works through
/// everything at `Extracted` — including an item that was already
/// waiting there — and writes the only rows the table holds afterwards.
async fn reset_reports_what_it_reembedded(side: Side, sock: PathBuf, root: PathBuf) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let mut id = 0;
    let sources = tempfile::tempdir()?;
    let paths = [
        write_source(sources.path(), "a")?,
        write_source(sources.path(), "b")?,
        write_source(sources.path(), "c")?,
    ];
    fill(&mut r, &mut w, &mut id, side, &paths).await?;
    let waiting = intakes(&root, side, IntakeStatus::Embedded)?[0];
    Catalog::open(&root.join(side.catalog))?.set_intake_status(
        side.kind,
        waiting,
        IntakeStatus::Extracted,
    )?;
    let embedded_before = intakes(&root, side, IntakeStatus::Embedded)?.len();
    let extracted_before = intakes(&root, side, IntakeStatus::Extracted)?.len();
    assert_eq!(
        (embedded_before, extracted_before),
        (2, 1),
        "the fixture needs items in both states"
    );

    id += 1;
    let reply = ok_result(&mut r, &mut w, id, side.reset, json!({ "yes": true })).await?;
    assert_eq!(reply["ok"], Value::Bool(true), "{reply}");
    assert_eq!(reply["library"], "default", "{reply}");
    assert_eq!(reply["resumed"], Value::Bool(false), "{reply}");
    let reembedded = reply["intakes_reembedded"]
        .as_u64()
        .with_context(|| format!("no intakes_reembedded: {reply}"))? as usize;
    let skipped = reply["skipped_empty"]
        .as_array()
        .with_context(|| format!("no skipped_empty: {reply}"))?
        .len();
    assert_eq!(
        reembedded + skipped,
        embedded_before + extracted_before,
        "{}: a fresh reset works through the demoted items and the ones already waiting: {reply}",
        side.reset
    );
    assert_eq!(
        reply["chunks_written"].as_u64().map(|n| n as usize),
        Some(chunk_rows(&root, side).await?),
        "{}: after a fresh reset the table holds only the rows it wrote: {reply}",
        side.reset
    );
    Ok(())
}

/// A resume reports this pass's share: the one item it picked up and
/// that item's rows, not the library's totals. The item it left alone
/// carries no audit row of the pass.
async fn reset_resume_reports_this_runs_share_only(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let mut id = 0;
    let sources = tempfile::tempdir()?;
    let paths = [
        write_source(sources.path(), "a")?,
        write_source(sources.path(), "b")?,
    ];
    fill(&mut r, &mut w, &mut id, side, &paths).await?;
    let embedded = intakes(&root, side, IntakeStatus::Embedded)?;
    assert_eq!(embedded.len(), 2, "the fixture embeds two items");
    let (target, untouched) = (embedded[0], embedded[1]);
    Catalog::open(&root.join(side.catalog))?.set_intake_status(
        side.kind,
        target,
        IntakeStatus::Extracted,
    )?;

    id += 1;
    let reply = ok_result(&mut r, &mut w, id, side.reset, json!({ "resume": true })).await?;
    assert_eq!(reply["resumed"], Value::Bool(true), "{reply}");
    assert_eq!(
        reply["intakes_reembedded"].as_u64(),
        Some(1),
        "{}: a resume counts the items it picked up: {reply}",
        side.reset
    );
    assert_eq!(
        reply["chunks_written"].as_u64().map(|n| n as usize),
        Some(chunk_rows_of(&root, side, target).await?),
        "{}: a resume counts the rows it wrote for its item: {reply}",
        side.reset
    );
    assert_eq!(intakes(&root, side, IntakeStatus::Embedded)?.len(), 2);
    assert!(
        reset_audited(&root, side, target)?,
        "the resumed item carries the pass's audit rows"
    );
    assert!(
        !reset_audited(&root, side, untouched)?,
        "the item already embedded must not be processed by the resume"
    );
    Ok(())
}

/// The index names on `side`'s chunks table and its sidecar bytes.
async fn index_state(root: &Path, side: Side) -> Result<(Vec<String>, Vec<u8>)> {
    let dir = root.join(side.lancedb);
    let indices = ChunkStore::try_open(&dir)
        .await?
        .context("the chunks table exists")?
        .list_indices()
        .await?;
    Ok((indices, std::fs::read(dir.join(META_FILENAME))?))
}

/// A one-partition index builds on a one-source library; asking the
/// same library for 256 partitions is refused as caller input with a
/// next step, and the index and sidecar the first build left are still
/// there byte for byte.
async fn rebuild_too_large_for_the_store_keeps_the_index(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let mut id = 0;
    let sources = tempfile::tempdir()?;
    fill(
        &mut r,
        &mut w,
        &mut id,
        side,
        &[write_source(sources.path(), "a")?],
    )
    .await?;

    id += 1;
    ok_result(
        &mut r,
        &mut w,
        id,
        side.rebuild,
        json!({ "kind": "ivf-flat", "num_partitions": 1 }),
    )
    .await?;
    let before = index_state(&root, side).await?;
    assert!(
        !before.0.is_empty(),
        "{}: the first build leaves an index: {:?}",
        side.rebuild,
        before.0
    );

    id += 1;
    let refused = call(
        &mut r,
        &mut w,
        id,
        side.rebuild,
        json!({ "kind": "ivf-flat", "num_partitions": 256 }),
    )
    .await?;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(-32602),
        "{}: a store too small for the index is caller input: {refused}",
        side.rebuild
    );
    let hint = refused["error"]["data"]["hint"]
        .as_str()
        .unwrap_or_default();
    assert!(
        hint.contains("num_partitions"),
        "{}: the hint names the way forward: {refused}",
        side.rebuild
    );
    assert_eq!(
        index_state(&root, side).await?,
        before,
        "{}: the refused rebuild must leave the index and its sidecar",
        side.rebuild
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_rebuild_too_large_for_the_store_keeps_the_index() -> Result<()> {
    with_daemon(|sock, root| rebuild_too_large_for_the_store_keeps_the_index(BOOKS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_rebuild_too_large_for_the_store_keeps_the_index() -> Result<()> {
    with_daemon(|sock, root| rebuild_too_large_for_the_store_keeps_the_index(PAPERS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_drop_reports_the_demoted_kind() -> Result<()> {
    with_daemon(|sock, root| drop_reports_the_demoted_kind(BOOKS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_drop_reports_the_demoted_kind() -> Result<()> {
    with_daemon(|sock, root| drop_reports_the_demoted_kind(PAPERS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stamps_reconcile_returns_the_stamps_it_wrote() -> Result<()> {
    with_daemon(|sock, root| reconcile_returns_the_stamps_it_wrote(BOOKS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_stamps_reconcile_returns_the_stamps_it_wrote() -> Result<()> {
    with_daemon(|sock, root| reconcile_returns_the_stamps_it_wrote(PAPERS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_resume_with_nothing_to_resume_reports_zero() -> Result<()> {
    with_daemon(|sock, root| reset_resume_with_nothing_to_resume_reports_zero(BOOKS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_resume_with_nothing_to_resume_reports_zero() -> Result<()> {
    with_daemon(|sock, root| reset_resume_with_nothing_to_resume_reports_zero(PAPERS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_reports_what_it_reembedded() -> Result<()> {
    with_daemon(|sock, root| reset_reports_what_it_reembedded(BOOKS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_reports_what_it_reembedded() -> Result<()> {
    with_daemon(|sock, root| reset_reports_what_it_reembedded(PAPERS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_resume_reports_this_runs_share_only() -> Result<()> {
    with_daemon(|sock, root| reset_resume_reports_this_runs_share_only(BOOKS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_resume_reports_this_runs_share_only() -> Result<()> {
    with_daemon(|sock, root| reset_resume_reports_this_runs_share_only(PAPERS, sock, root)).await
}
