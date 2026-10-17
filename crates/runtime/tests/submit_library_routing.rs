// SPDX-License-Identifier: Apache-2.0

//! The three queue-submitting methods — `ingest.submit`,
//! `glean.submit`, `intake.ocr` — resolve their target library
//! through the registry at enqueue time.
//!
//! An unnamed submit lands on the registry's current default pointer,
//! not on the library the daemon came up under; a submit naming a
//! library the registry does not hold is refused with `-32010` before
//! a job is written; and the name a job carries is fixed when it is
//! enqueued, so moving the default pointer afterwards does not move
//! the job.
//!
//! Two libraries: `alpha` is the registry default, and the daemon is
//! brought up under `beta`, so the primary and the default differ —
//! which is what makes the fallback's source observable. The queue is
//! paused before anything is submitted, so the rows stay pending and
//! the assertions read a stable queue. The embedder probe daemon
//! bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::sync::OnceLock;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, connect, join_with_deadline, recv, send};

/// Isolate the process and seed a two-library registry: `alpha` (the
/// registry default) and `beta`, each with its own data root under the
/// sandbox.
fn world() -> &'static Sandbox {
    static SEEDED: OnceLock<()> = OnceLock::new();
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    SEEDED.get_or_init(|| {
        let alpha = sandbox.data_root("alpha-root");
        let beta = sandbox.data_root("beta-root");
        sandbox.write_registry_entries(
            Some("alpha"),
            &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
        );
    });
    sandbox
}

/// Regular files for the submit parameters that check their inputs
/// exist before enqueueing: a text book, an OCR markdown product and
/// the scan it claims to come from.
struct Fixtures {
    book: std::path::PathBuf,
    ocr_md: std::path::PathBuf,
    from_pdf: std::path::PathBuf,
}

fn fixtures(dir: &Path) -> Result<Fixtures> {
    let book = dir.join("synthetic-ledger.txt");
    std::fs::write(&book, "Chapter One\n\nA synthetic ledger.\n")?;
    let ocr_md = dir.join("scan.md");
    std::fs::write(&ocr_md, "<!-- page 1 (sheet 1) -->\n\nA synthetic page.\n")?;
    let from_pdf = dir.join("scan.pdf");
    std::fs::write(&from_pdf, b"%PDF-1.4\n%%EOF\n")?;
    Ok(Fixtures {
        book,
        ocr_md,
        from_pdf,
    })
}

/// The three submit requests, each with `library` set to `library`
/// when given and omitted otherwise.
fn submits(f: &Fixtures, library: Option<&str>) -> Vec<(&'static str, Value)> {
    let with_library = |mut params: Value| {
        if let Some(name) = library {
            params["library"] = json!(name);
        }
        params
    };
    vec![
        ("ingest.submit", with_library(json!({ "paths": [f.book] }))),
        (
            "glean.submit",
            with_library(json!({ "paths": [f.book], "force": true })),
        ),
        (
            "intake.ocr",
            with_library(json!({
                "ocr_md": f.ocr_md,
                "from_pdf": f.from_pdf,
                "expected_pages": 1,
                "force": true,
            })),
        ),
    ]
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

/// The job id(s) a successful submit answered with.
fn job_ids(method: &str, resp: &Value) -> Result<Vec<String>> {
    let result = &resp["result"];
    let ids: Vec<String> = match method {
        "intake.ocr" => vec![
            result["job_id"]
                .as_str()
                .with_context(|| format!("{method}: no job_id in {resp}"))?
                .to_string(),
        ],
        _ => result["job_ids"]
            .as_array()
            .with_context(|| format!("{method}: no job_ids in {resp}"))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    };
    if ids.is_empty() {
        return Err(eyre!("{method}: submit answered with no job ids: {resp}"));
    }
    Ok(ids)
}

/// `queue.list`'s rows, keyed by job id.
async fn queue_rows(reader: &mut Reader, w: &mut Writer, id: u64) -> Result<Vec<Value>> {
    let resp = call(reader, w, id, "queue.list", Value::Null).await?;
    resp["result"]["jobs"]
        .as_array()
        .cloned()
        .with_context(|| format!("queue.list carries no jobs array: {resp}"))
}

fn library_of<'a>(rows: &'a [Value], job_id: &str) -> Result<&'a str> {
    rows.iter()
        .find(|row| row["id"].as_str() == Some(job_id))
        .with_context(|| format!("job {job_id} not in queue.list"))?["library"]
        .as_str()
        .with_context(|| format!("job {job_id} carries no library"))
}

/// Submit all three methods without a library and assert every queued
/// row names `expected`.
async fn unnamed_submits_land_on(
    reader: &mut Reader,
    w: &mut Writer,
    next_id: &mut u64,
    f: &Fixtures,
    expected: &str,
) -> Result<Vec<String>> {
    let mut all_ids = Vec::new();
    for (method, params) in submits(f, None) {
        *next_id += 1;
        let resp = call(reader, w, *next_id, method, params).await?;
        all_ids.extend(job_ids(method, &resp)?.into_iter().map(|id| (method, id)));
    }
    *next_id += 1;
    let rows = queue_rows(reader, w, *next_id).await?;
    for (method, id) in &all_ids {
        assert_eq!(
            library_of(&rows, id)?,
            expected,
            "{method}: an unnamed submit must land on the registry's current default"
        );
    }
    Ok(all_ids.into_iter().map(|(_, id)| id).collect())
}

/// An unnamed submit resolves to the registry's current default, and
/// follows the pointer when `library.set_default` moves it; a job
/// already queued keeps the name it was enqueued with.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unnamed_submit_lands_on_the_registry_default() -> Result<()> {
    world();
    let runtime_root = tempfile::tempdir()?;
    let fixture_dir = tempfile::tempdir()?;
    let f = fixtures(fixture_dir.path())?;

    // `alpha` is the registry default; come up under `beta`.
    let mut opts = RuntimeOpts::headless(None, Some("beta".to_string()));
    opts.no_mcp = true;
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    assert_eq!(
        runtime.registry.get(None)?.name(),
        "alpha",
        "the fixture's default pointer has to differ from the primary \
         for this test to discriminate"
    );

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let mut id = 0u64;

        // Control: the daemon itself reports the primary and the
        // default on different rows, so a wrong answer below is the
        // fallback's doing and not the fixture's.
        id += 1;
        let status = call(&mut reader, &mut w, id, "daemon.status", Value::Null).await?;
        assert_eq!(status["result"]["library"], "beta", "{status}");
        let served = status["result"]["served"]
            .as_array()
            .with_context(|| format!("status carries no served rows: {status}"))?;
        let default_row = served
            .iter()
            .find(|row| row["default"] == Value::Bool(true))
            .with_context(|| format!("no default row in served: {status}"))?;
        assert_eq!(default_row["name"], "alpha", "{status}");

        // Pause first, so the rows stay pending for `queue.list`.
        id += 1;
        let paused = call(&mut reader, &mut w, id, "queue.pause", Value::Null).await?;
        assert_eq!(paused["result"]["paused"], Value::Bool(true), "{paused}");

        let first = unnamed_submits_land_on(&mut reader, &mut w, &mut id, &f, "alpha").await?;

        // Move the pointer at run time: new unnamed submits follow it.
        id += 1;
        let flipped = call(
            &mut reader,
            &mut w,
            id,
            "library.set_default",
            json!({ "name": "beta" }),
        )
        .await?;
        assert_eq!(flipped["result"]["ok"], Value::Bool(true), "{flipped}");

        let _second = unnamed_submits_land_on(&mut reader, &mut w, &mut id, &f, "beta").await?;

        // The rows queued before the flip keep the name they were
        // enqueued with.
        id += 1;
        let rows = queue_rows(&mut reader, &mut w, id).await?;
        for job in &first {
            assert_eq!(
                library_of(&rows, job)?,
                "alpha",
                "a queued job must keep its library when the default pointer moves"
            );
        }

        // Restore the registry default for any other test sharing the
        // sandbox.
        id += 1;
        let restored = call(
            &mut reader,
            &mut w,
            id,
            "library.set_default",
            json!({ "name": "alpha" }),
        )
        .await?;
        assert_eq!(restored["result"]["ok"], Value::Bool(true), "{restored}");

        id += 1;
        let _ = call(&mut reader, &mut w, id, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}

/// A submit naming a library the registry does not hold is refused
/// with `-32010` and writes no row; a submit naming a mounted library
/// that is not the default lands on that library.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_submit_naming_an_unknown_library_is_refused_before_enqueueing() -> Result<()> {
    world();
    let runtime_root = tempfile::tempdir()?;
    let fixture_dir = tempfile::tempdir()?;
    let f = fixtures(fixture_dir.path())?;

    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    let queue_state_path = bookrack_config::daemon_state_dir()
        .map_err(|e| eyre!("resolve the daemon state directory: {e}"))?
        .join("queue.json");

    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let mut id = 0u64;

        id += 1;
        let paused = call(&mut reader, &mut w, id, "queue.pause", Value::Null).await?;
        assert_eq!(paused["result"]["paused"], Value::Bool(true), "{paused}");

        id += 1;
        let before = queue_rows(&mut reader, &mut w, id).await?.len();
        let disk_before = std::fs::read_to_string(&queue_state_path).unwrap_or_default();

        for (method, params) in submits(&f, Some("no-such-library")) {
            id += 1;
            let resp = call(&mut reader, &mut w, id, method, params).await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(-32010),
                "{method}: an unknown library must be refused before enqueueing: {resp}"
            );
            assert!(
                resp["result"].is_null(),
                "{method}: a refused submit must not answer with job ids: {resp}"
            );
        }

        id += 1;
        let after = queue_rows(&mut reader, &mut w, id).await?.len();
        assert_eq!(
            after, before,
            "a refused submit must not add a row to the in-memory queue"
        );
        let disk_after = std::fs::read_to_string(&queue_state_path).unwrap_or_default();
        assert_eq!(
            disk_after, disk_before,
            "a refused submit must not rewrite the queue document on disk"
        );

        // An explicit, mounted, non-default name is honoured as given.
        let mut named = Vec::new();
        for (method, params) in submits(&f, Some("beta")) {
            id += 1;
            let resp = call(&mut reader, &mut w, id, method, params).await?;
            named.extend(job_ids(method, &resp)?.into_iter().map(|j| (method, j)));
        }
        id += 1;
        let rows = queue_rows(&mut reader, &mut w, id).await?;
        for (method, job) in &named {
            assert_eq!(
                library_of(&rows, job)?,
                "beta",
                "{method}: an explicit library must be the one the job carries"
            );
        }

        id += 1;
        let _ = call(&mut reader, &mut w, id, "daemon.shutdown", Value::Null).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
