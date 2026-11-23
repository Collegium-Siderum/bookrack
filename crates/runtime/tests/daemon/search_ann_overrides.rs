// SPDX-License-Identifier: Apache-2.0

//! The per-call ANN overrides a control-plane search carries are the
//! ones the search runs with.
//!
//! `bypass_index`, `nprobes`, and `refine_factor` are optional fields of
//! the search methods' params, which ignore a field they do not know. A
//! call whose overrides were dropped on the way in would therefore answer
//! like one that honoured them: on a library small enough to have no ANN
//! index, the search is exhaustive either way. What tells the two apart
//! is the call's `mcp_tool_calls` row, whose `overrides` the search
//! records from the options it ran with. Each search method's row
//! carries the values its JSON named, a `kind: "all"` search carries them
//! once per side, and a call that misspells them records none.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::Catalog;
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

/// An id no paper in this fixture carries.
const PHANTOM: i64 = 999_999;

fn write_source(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("synthetic-ledger.txt");
    std::fs::write(
        &path,
        "Chapter One\n\n\
         The synthetic ledger recorded the tides of a fictional harbour. \
         Every tide had a fictional height, and every height was written \
         beside the hour the fictional keeper had measured it.\n\n\
         By evening the ledger was complete. The keeper closed it and \
         recorded the closing as a footnote.\n",
    )?;
    Ok(path)
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

/// Submit one book and wait for its job to be `done`.
async fn fill_one(reader: &mut Reader, w: &mut Writer, path: &Path) -> Result<()> {
    let resp = call(reader, w, 1, "ingest.submit", json!({ "paths": [path] })).await?;
    let job = resp["result"]["job_ids"][0]
        .as_str()
        .with_context(|| format!("no job id: {resp}"))?
        .to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        let listed = call(reader, w, 2, "queue.list", Value::Null).await?;
        let state = listed["result"]["jobs"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(&job)))
            .and_then(|row| row["state"].as_str())
            .unwrap_or("missing")
            .to_string();
        match state.as_str() {
            "done" => return Ok(()),
            "failed" | "cancelled" | "missing" => {
                return Err(eyre!("the fixture job did not finish clean: {listed}"));
            }
            _ if std::time::Instant::now() > deadline => {
                return Err(eyre!("the fixture job is still {state}"));
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Ingest one book, then run every search method once with overrides of
/// its own, and `library.search` once more with the three names
/// misspelt.
async fn run_searches(sock: PathBuf) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, &write_source(sources.path())?).await?;

    let finding = [
        (
            "library.search",
            json!({ "query": "tides", "bypass_index": true, "nprobes": 4, "refine_factor": 2 }),
        ),
        (
            "library.search",
            json!({ "query": "tides", "nprobe": 4, "bypassIndex": true, "refineFactor": 2 }),
        ),
        (
            "library.search",
            json!({ "query": "tides", "kind": "all", "nprobes": 6, "refine_factor": 3 }),
        ),
        (
            "library.search_in_book",
            json!({ "query": "tides", "intake_id": 1, "bypass_index": true, "nprobes": 5 }),
        ),
    ];
    for (id, (method, params)) in (10u64..).zip(finding) {
        let reply = call(&mut r, &mut w, id, method, params.clone()).await?;
        let hits = reply["result"]
            .as_array()
            .with_context(|| format!("{method} {params}: no result: {reply}"))?;
        if hits.is_empty() {
            return Err(eyre!("{method} {params}: the fixture book is not found"));
        }
    }
    // No paper is ingested, so this search finds nothing; the call is
    // recorded whatever it answers.
    call(
        &mut r,
        &mut w,
        20,
        "library.search_in_paper",
        json!({ "query": "tides", "intake_id": PHANTOM, "refine_factor": 7 }),
    )
    .await?;
    Ok(())
}

/// The `overrides` each recorded call of `tool` carries, oldest first.
fn recorded_overrides(catalog: &Catalog, tool: &str) -> Result<Vec<Value>> {
    catalog
        .tool_calls_for_tool(tool)?
        .into_iter()
        .map(|row| {
            let args: Value = serde_json::from_str(
                row.args
                    .as_deref()
                    .with_context(|| format!("a {tool} row carries no arguments"))?,
            )?;
            Ok(args["overrides"].clone())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_search_method_runs_with_the_ann_overrides_its_json_names() -> Result<()> {
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
        let result = run_searches(sock.clone()).await;
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
    join_with_deadline(runtime, repl_handle, driver).await?;

    let catalog = Catalog::open_for_write_unbacked(&data_root.path().join("catalog.db"))
        .map_err(|e| eyre!("reopen catalog: {e}"))?;
    assert_eq!(
        recorded_overrides(&catalog, "library.search")?,
        [
            json!({ "bypass_index": true, "nprobes": 4, "refine_factor": 2 }),
            json!({}),
        ],
        "the book search runs with the three overrides, and with none when they are misspelt"
    );
    assert_eq!(
        recorded_overrides(&catalog, "library.search.unified")?,
        [json!({
            "books": { "nprobes": 6, "refine_factor": 3 },
            "papers": { "nprobes": 6, "refine_factor": 3 },
        })],
        "a search of both sides runs each with the overrides"
    );
    assert_eq!(
        recorded_overrides(&catalog, "library.search_in_book")?,
        [json!({ "bypass_index": true, "nprobes": 5 })],
        "the one-book search runs with its overrides"
    );
    assert_eq!(
        recorded_overrides(&catalog, "library.search_in_paper")?,
        [json!({ "refine_factor": 7 })],
        "the one-paper search runs with its override"
    );
    Ok(())
}
