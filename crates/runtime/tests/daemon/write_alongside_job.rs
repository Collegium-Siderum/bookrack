// SPDX-License-Identifier: Apache-2.0

//! A control-plane write runs alongside a queue job on the same
//! library. The queue worker does not take the daemon's write mutex,
//! so a curation write issued while an ingest or a glean is embedding
//! returns at once; the one exception is `papers.metadata.reaudit`,
//! which also takes the library's glean lock and so waits for a
//! running glean to end.
//!
//! The job is held mid-embed by the shared stub's hold switch: its
//! source carries `EmbedStub::HOLD_MARKER` in every paragraph, so the
//! worker's first embed request parks on the stub until the test drops
//! the hold. The edited intake is seeded straight into the catalog
//! before bring-up, so the write under test never waits on the job to
//! create it.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::ItemKind;
use bookrack_test_support::{EmbedStub, ProcessEnv, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

/// How long a test waits for the held job's embed request to park.
const PARK_WITHIN: Duration = Duration::from_secs(30);

/// How long `papers.metadata.reaudit` must stay unanswered while the
/// glean it waits on is held.
const STILL_WAITING: Duration = Duration::from_secs(2);

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

/// A synthetic text whose every paragraph carries the hold marker, so
/// whichever batch the worker embeds first is held.
fn write_held_source(dir: &Path, tag: &str) -> Result<PathBuf> {
    let marker = EmbedStub::HOLD_MARKER;
    let path = dir.join(format!("held-ledger-{tag}.txt"));
    std::fs::write(
        &path,
        format!(
            "Chapter One\n\n\
             The synthetic narrator opened the {marker} ledger numbered {tag} and \
             began to count the fictional weights it described, one entry at a time.\n\n\
             By noon the {marker} ledger was balanced, and the narrator recorded the \
             balance as a footnote beside the last fictional weight.\n\n\
             Chapter Two\n\n\
             On the second day the {marker} ledger grew a column of colours, which the \
             narrator sorted from dull to bright before closing the book.\n"
        ),
    )?;
    Ok(path)
}

/// Seed one intake of `kind` into `catalog` and return its id.
fn seed_intake(catalog: &Path, kind: ItemKind, sha: &str, format: &str) -> Result<i64> {
    Ok(Catalog::open_for_write_unbacked(catalog)?
        .register_intake(kind, &NewIntake::new(sha).format(format))?
        .into_intake()
        .intake_id)
}

/// The state `queue.list` reports for `job`.
async fn job_state(reader: &mut Reader, w: &mut Writer, id: u64, job: &str) -> Result<String> {
    let listed = ok_result(reader, w, id, "queue.list", Value::Null).await?;
    Ok(listed["jobs"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(job)))
        .and_then(|row| row["state"].as_str())
        .unwrap_or("missing")
        .to_string())
}

/// Submit `path` through `method` and return the one job id it queued.
async fn submit(
    reader: &mut Reader,
    w: &mut Writer,
    id: u64,
    method: &str,
    path: &Path,
) -> Result<String> {
    let submitted = ok_result(reader, w, id, method, json!({ "paths": [path] })).await?;
    Ok(submitted["job_ids"][0]
        .as_str()
        .with_context(|| format!("no job id: {submitted}"))?
        .to_string())
}

/// Poll until `job` is `done`; any other terminal state is a failure.
async fn wait_done(reader: &mut Reader, w: &mut Writer, job: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    let mut id = 1_000;
    loop {
        id += 1;
        let state = job_state(reader, w, id, job).await?;
        match state.as_str() {
            "done" => return Ok(()),
            "failed" | "cancelled" | "missing" => {
                return Err(eyre!("the held job ended {state}"));
            }
            _ if std::time::Instant::now() > deadline => {
                return Err(eyre!("the held job is still {state}"));
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Block this worker thread until `n` embed requests park on `hold`.
fn parked(hold: &bookrack_test_support::HeldEmbeds, n: usize) -> bool {
    tokio::task::block_in_place(|| hold.wait_parked(n, PARK_WITHIN))
}

/// Seed with `seed` before bring-up, bring a daemon up on that root,
/// hand `body` the socket, the root, and what `seed` returned, then
/// shut it down.
async fn with_seeded_daemon<S, F, Fut>(seed: S, body: F) -> Result<()>
where
    S: FnOnce(&Path) -> Result<i64>,
    F: FnOnce(PathBuf, PathBuf, i64) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let seeded = seed(data_root.path())?;
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
        let result = body(sock.clone(), root, seeded).await;
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

/// A book-side metadata write returns while an ingest job on the same
/// library is held mid-embed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_book_metadata_write_returns_while_an_ingest_is_embedding() -> Result<()> {
    with_seeded_daemon(
        |root| {
            seed_intake(
                &root.join("catalog.db"),
                ItemKind::Book,
                "sha-seeded-book",
                "epub",
            )
        },
        |sock, root, book| async move {
            let (mut r, mut w) = connect(&sock).await?;
            let hold = EmbedStub::shared().hold_marked();
            let source = write_held_source(&root, "book")?;
            let job = submit(&mut r, &mut w, 1, "ingest.submit", &source).await?;
            assert!(parked(&hold, 1), "the ingest never reached its embed");
            assert_eq!(job_state(&mut r, &mut w, 2, &job).await?, "running");

            let set = call(
                &mut r,
                &mut w,
                3,
                "metadata.set",
                json!({ "book": book, "field": "title", "value": "Edited Alongside" }),
            )
            .await?;
            assert!(
                set["error"].is_null(),
                "a book metadata write must not wait for or refuse on a running ingest: {set}"
            );
            assert_eq!(
                job_state(&mut r, &mut w, 4, &job).await?,
                "running",
                "the write returned only after the ingest left its embed"
            );

            drop(hold);
            wait_done(&mut r, &mut w, &job).await
        },
    )
    .await
}

/// A paper-side metadata write returns while a glean job on the same
/// library is held mid-embed, writing the paper catalog that glean is
/// writing; `papers.metadata.reaudit` alone waits for the glean, on the
/// library's glean lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_metadata_write_returns_while_a_glean_is_embedding_and_reaudit_waits() -> Result<()>
{
    with_seeded_daemon(
        |root| {
            seed_intake(
                &root.join("papers_catalog.db"),
                ItemKind::Paper,
                "sha-seeded-paper",
                "pdf",
            )
        },
        |sock, root, paper| async move {
            let (mut ra, mut wa) = connect(&sock).await?;
            let hold = EmbedStub::shared().hold_marked();
            let source = write_held_source(&root, "paper")?;
            let job = submit(&mut ra, &mut wa, 1, "glean.submit", &source).await?;
            assert!(parked(&hold, 1), "the glean never reached its embed");
            assert_eq!(job_state(&mut ra, &mut wa, 2, &job).await?, "running");

            let set = call(
                &mut ra,
                &mut wa,
                3,
                "papers.metadata.set",
                json!({ "intake_id": paper, "field": "title", "value": "Edited Alongside" }),
            )
            .await?;
            assert!(
                set["error"].is_null(),
                "a paper metadata write must not wait for or refuse on a running glean: {set}"
            );
            assert_eq!(
                job_state(&mut ra, &mut wa, 4, &job).await?,
                "running",
                "the write returned only after the glean left its embed"
            );

            // A connection answers its requests in order, so the
            // re-audit goes out on a second one: stuck on the glean
            // lock, it would hold up a `queue.list` behind it.
            let (mut rb, mut wb) = connect(&sock).await?;
            let req = json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "papers.metadata.reaudit",
                "params": { "intake_id": paper },
            });
            send(&mut wb, &req.to_string()).await?;
            let early = tokio::time::timeout(STILL_WAITING, recv(&mut rb)).await;
            assert!(
                early.is_err(),
                "papers.metadata.reaudit answered while the glean held its lock: {early:?}"
            );
            assert_eq!(job_state(&mut ra, &mut wa, 6, &job).await?, "running");

            drop(hold);
            let reaudit = tokio::time::timeout(Duration::from_secs(30), recv(&mut rb))
                .await
                .map_err(|_| eyre!("papers.metadata.reaudit never answered after the release"))??;
            assert_eq!(reaudit["id"].as_u64(), Some(5), "{reaudit}");
            wait_done(&mut ra, &mut wa, &job).await
        },
    )
    .await
}
