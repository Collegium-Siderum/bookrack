// SPDX-License-Identifier: Apache-2.0

//! A vector reset that fails tells the caller whether resuming is the
//! next step.
//!
//! A failure inside the reset pass leaves finished items at `Embedded`
//! and the rest at `Extracted`, so the error's hint gains the resumed
//! rerun after whatever step the cause itself calls for. A failure
//! while the reset is being set up changed nothing — a resume there
//! would skip a reset that never ran — so its hint is left alone and
//! no run is registered.
//!
//! A library whose index profile declares a dimension the backend does
//! not produce is refused before the reset touches anything: the store,
//! the stamps, and the catalog are left exactly as they were, and the
//! refusal is caller input with no resume step.
//!
//! `EmbedStub::set_failure` is process-global, so this file relies on
//! the per-test process `cargo nextest` runs it in; under a plain
//! `cargo test` its flipped switch reaches every test in the binary.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::{
    DEFAULT_EMBED_MODEL, LibraryKind, ManifestIdentitySeed, set_manifest_index_profile,
};
use bookrack_corpus::{
    CHUNK_VERSION_KEY, Corpus, EMBED_MODEL_KEY, NORMALIZE_VERSION_KEY, VECTOR_DIM_KEY,
};
use bookrack_index_profile::USER_PROFILE_DIR_NAME;
use bookrack_test_support::{EmbedFailure, EmbedStub, ProcessEnv, process_env};
use bookrack_vectors::ChunkStore;
use eyre::{ContextCompat, Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};

/// One pipeline's reset surface.
#[derive(Clone, Copy)]
struct Side {
    submit: &'static str,
    reset: &'static str,
    reembed: &'static str,
    catalog: &'static str,
    /// The corpus database the side's stamps live in.
    corpus: &'static str,
    /// The side's vector store directory.
    lancedb: &'static str,
    /// The `pipeline_runs.command` a reset registers under.
    reset_run: &'static str,
}

const BOOKS: Side = Side {
    submit: "ingest.submit",
    reset: "vectors.reset",
    reembed: "vectors.reembed",
    catalog: "catalog.db",
    corpus: "corpus.db",
    lancedb: "lancedb",
    reset_run: "reset",
};

const PAPERS: Side = Side {
    submit: "glean.submit",
    reset: "papers.vectors_reset",
    reembed: "papers.vectors_reembed",
    catalog: "papers_catalog.db",
    corpus: "papers_corpus.db",
    lancedb: "lancedb_papers",
    reset_run: "papers_reset",
};

/// A dimension no stub-backed library can be built at: the stub answers
/// every model at [`EmbedStub::DIMENSION`].
const UNREACHABLE_DIM: u32 = 1024;

fn write_source(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("synthetic-ledger.txt");
    std::fs::write(
        &path,
        "Chapter One\n\n\
         The synthetic narrator opened the synthetic ledger and began to \
         count. Every entry in the ledger described a fictional object, and \
         every fictional object had a fictional weight. Counting the weights \
         took the narrator most of the morning.\n\n\
         By noon the ledger was balanced. The narrator celebrated with a \
         fictional meal and recorded the celebration as a footnote.\n",
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

/// The `error` of a call that must fail.
async fn error_of(
    reader: &mut Reader,
    w: &mut Writer,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let resp = call(reader, w, id, method, params).await?;
    if resp["error"].is_null() {
        return Err(eyre!("{method} was expected to fail: {resp}"));
    }
    Ok(resp["error"].clone())
}

/// Submit one source through `side` and wait for its job to be `done`.
async fn fill_one(reader: &mut Reader, w: &mut Writer, side: Side, path: &Path) -> Result<()> {
    let resp = call(reader, w, 1, side.submit, json!({ "paths": [path] })).await?;
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

/// A brute-force profile declaring `model` at `dim`, in the per-user
/// profile file format.
fn profile_toml(name: &str, model: &str, dim: u32) -> String {
    format!(
        "schema_version = 1\n\
         name = \"{name}\"\n\
         description = \"A synthetic profile.\"\n\
         \n\
         [embed]\n\
         backend = \"ollama\"\n\
         model = \"{model}\"\n\
         dim = {dim}\n\
         \n\
         [ann]\n\
         kind = \"brute-force\"\n\
         num_partitions = 1\n\
         num_bits = 8\n\
         nprobes = 1\n\
         refine_factor = 1\n\
         \n\
         [reranker]\n\
         kind = \"none\"\n"
    )
}

/// Write a user profile beside the sandbox registry and point the
/// library at `root` to it. The reset handler resolves the reference
/// through the manifest at execution time, as `index-profile apply`
/// leaves it.
fn declare_profile(root: &Path, name: &str, model: &str, dim: u32) -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon());
    let dir = sandbox
        .registry_path()
        .parent()
        .context("the sandbox registry has a parent")?
        .join(USER_PROFILE_DIR_NAME);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(format!("{name}.toml")),
        profile_toml(name, model, dim),
    )?;
    set_manifest_index_profile(
        root,
        Some(name),
        ManifestIdentitySeed {
            name: "default",
            kind: LibraryKind::Test,
            description: None,
        },
    )?;
    Ok(())
}

/// Everything a reset could change on one side: the four index stamps,
/// how many intakes sit at each embed state, the persisted ANN
/// configuration, and the chunk count.
#[derive(Debug, PartialEq, Eq)]
struct SideState {
    stamps: [Option<String>; 4],
    embedded: usize,
    extracted: usize,
    ann_meta: Option<String>,
    chunks: usize,
}

async fn side_state(root: &Path, side: Side) -> Result<SideState> {
    let corpus = Corpus::open_read_only(&root.join(side.corpus))?;
    let mut stamps: [Option<String>; 4] = Default::default();
    for (slot, key) in stamps.iter_mut().zip([
        EMBED_MODEL_KEY,
        VECTOR_DIM_KEY,
        CHUNK_VERSION_KEY,
        NORMALIZE_VERSION_KEY,
    ]) {
        *slot = corpus.meta_get(key)?;
    }
    let catalog = Catalog::open_read_only(&root.join(side.catalog))?;
    let embedded = catalog.intakes_with_status(IntakeStatus::Embedded)?.len();
    let extracted = catalog.intakes_with_status(IntakeStatus::Extracted)?.len();
    let lancedb = root.join(side.lancedb);
    let ann_meta =
        std::fs::read_to_string(lancedb.join(bookrack_vectors::meta::META_FILENAME)).ok();
    let chunks = match ChunkStore::try_open(&lancedb).await? {
        Some(store) => store.count_rows().await?,
        None => 0,
    };
    Ok(SideState {
        stamps,
        embedded,
        extracted,
        ann_meta,
        chunks,
    })
}

fn reset_runs(root: &Path, side: Side) -> Result<usize> {
    Ok(Catalog::open(&root.join(side.catalog))?
        .list_pipeline_runs(None, None)?
        .into_iter()
        .filter(|run| run.command == side.reset_run)
        .count())
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

/// The embedder goes down partway through the pass. The envelope is
/// the one the same backend failure takes on a re-embed, a write with
/// no reset behind it — same code, summary, evidence, and retry
/// judgement — and its hint keeps that failure's own step first and
/// adds the resumed rerun after it.
async fn a_reset_interrupted_mid_pass_adds_the_resume_step(
    side: Side,
    sock: PathBuf,
    _root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, side, &write_source(sources.path())?).await?;

    // The dimension probe is answered from a process-wide cache once
    // the daemon has made it, so the comparison write has to embed
    // chunk text for real: the re-embed execute leg does.
    let plan = call(&mut r, &mut w, 3, side.reembed, json!({ "dry_run": true })).await?;
    let plan_id = plan["result"]["plan_id"]
        .as_str()
        .with_context(|| format!("no plan id: {plan}"))?
        .to_string();
    EmbedStub::set_failure(EmbedFailure::Overloaded);
    let plain = error_of(
        &mut r,
        &mut w,
        4,
        side.reembed,
        json!({ "yes": true, "plan_id": plan_id }),
    )
    .await?;
    let reset = error_of(&mut r, &mut w, 5, side.reset, json!({ "yes": true })).await?;
    EmbedStub::set_failure(EmbedFailure::None);

    assert_eq!(reset["code"], plain["code"], "{reset} vs {plain}");
    assert_eq!(reset["message"], plain["message"], "{reset} vs {plain}");
    assert_eq!(reset["data"]["detail"], plain["data"]["detail"]);
    assert_eq!(reset["data"]["retryable"], plain["data"]["retryable"]);
    let own_step = plain["data"]["hint"]
        .as_str()
        .with_context(|| format!("the backend failure writes its own hint: {plain}"))?;
    let hint = reset["data"]["hint"]
        .as_str()
        .with_context(|| format!("no hint: {reset}"))?;
    assert!(
        hint.starts_with(own_step) && hint.len() > own_step.len(),
        "{}: the failure's own step comes first, the resume after it: {hint:?}",
        side.reset
    );
    assert!(hint.contains("--resume"), "{}: {hint:?}", side.reset);
    Ok(())
}

/// The reset cannot resolve its embedding configuration, so nothing
/// was changed: no resume is offered and no run is registered.
async fn a_reset_that_never_started_offers_no_resume(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    set_manifest_index_profile(
        &root,
        Some("no-such-profile"),
        ManifestIdentitySeed {
            name: "default",
            kind: LibraryKind::Test,
            description: None,
        },
    )?;
    let (mut r, mut w) = connect(&sock).await?;
    let err = error_of(&mut r, &mut w, 1, side.reset, json!({ "yes": true })).await?;
    assert_eq!(
        err["code"].as_i64(),
        Some(-32602),
        "{}: an undefined profile is caller input: {err}",
        side.reset
    );
    let hint = err["data"]["hint"].as_str().unwrap_or_default();
    assert!(
        !hint.contains("--resume"),
        "{}: nothing ran, so there is nothing to resume: {err}",
        side.reset
    );
    assert_eq!(reset_runs(&root, side)?, 0, "{}", side.reset);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_interrupted_mid_pass_adds_the_resume_step() -> Result<()> {
    with_daemon(|sock, root| a_reset_interrupted_mid_pass_adds_the_resume_step(BOOKS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_interrupted_mid_pass_adds_the_resume_step() -> Result<()> {
    with_daemon(|sock, root| a_reset_interrupted_mid_pass_adds_the_resume_step(PAPERS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_that_never_started_offers_no_resume() -> Result<()> {
    with_daemon(|sock, root| a_reset_that_never_started_offers_no_resume(BOOKS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_that_never_started_offers_no_resume() -> Result<()> {
    with_daemon(|sock, root| a_reset_that_never_started_offers_no_resume(PAPERS, sock, root)).await
}

/// The profile declares a dimension the backend does not produce. The
/// reset is refused as caller input before it clears a stamp or drops
/// the table: nothing the reset could change has moved, no run is
/// registered, and there is no resume to offer.
async fn a_reset_against_an_unreachable_dimension_changes_nothing(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, side, &write_source(sources.path())?).await?;
    let before = side_state(&root, side).await?;
    let profile = format!("{}-unreachable", side.reset_run.replace('_', "-"));
    declare_profile(&root, &profile, DEFAULT_EMBED_MODEL, UNREACHABLE_DIM)?;

    let err = error_of(&mut r, &mut w, 3, side.reset, json!({ "yes": true })).await?;
    assert_eq!(
        err["code"].as_i64(),
        Some(-32602),
        "{}: a declared dimension the backend does not produce is caller input: {err}",
        side.reset
    );
    let message = err["message"].as_str().unwrap_or_default();
    for needle in [
        profile.as_str(),
        &UNREACHABLE_DIM.to_string(),
        &format!("{}-dimensional", EmbedStub::DIMENSION),
    ] {
        assert!(
            message.contains(needle),
            "{}: {needle:?} missing from {message:?}",
            side.reset
        );
    }
    let hint = err["data"]["hint"].as_str().unwrap_or_default();
    assert!(
        !hint.contains("--resume"),
        "{}: nothing ran, so there is nothing to resume: {err}",
        side.reset
    );
    assert_eq!(reset_runs(&root, side)?, 0, "{}", side.reset);
    assert_eq!(
        side_state(&root, side).await?,
        before,
        "{}: the refused reset changed the library",
        side.reset
    );
    Ok(())
}

/// A reset stopped partway, then resumed under a profile whose
/// dimension the backend does not produce: the resume would finish a
/// library that can never match, so it is refused the same way and the
/// waiting intakes stay where they are.
async fn a_resumed_reset_against_an_unreachable_dimension_is_refused(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, side, &write_source(sources.path())?).await?;
    EmbedStub::set_failure(EmbedFailure::Overloaded);
    let interrupted = error_of(&mut r, &mut w, 3, side.reset, json!({ "yes": true })).await;
    EmbedStub::set_failure(EmbedFailure::None);
    interrupted?;
    let before = side_state(&root, side).await?;
    assert!(
        before.extracted > 0,
        "{}: the fixture reset must leave an intake waiting: {before:?}",
        side.reset
    );

    let profile = format!("{}-unreachable-resume", side.reset_run.replace('_', "-"));
    declare_profile(&root, &profile, DEFAULT_EMBED_MODEL, UNREACHABLE_DIM)?;
    let err = error_of(
        &mut r,
        &mut w,
        4,
        side.reset,
        json!({ "yes": true, "resume": true }),
    )
    .await?;
    assert_eq!(err["code"].as_i64(), Some(-32602), "{}: {err}", side.reset);
    assert_eq!(
        side_state(&root, side).await?,
        before,
        "{}: the refused resume changed the library",
        side.reset
    );
    Ok(())
}

/// A resume with nothing waiting returns before the dimension check, so
/// even a profile the backend cannot satisfy reports the zero pass.
async fn a_resume_with_nothing_waiting_skips_the_check(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, side, &write_source(sources.path())?).await?;
    let profile = format!("{}-unreachable-idle", side.reset_run.replace('_', "-"));
    declare_profile(&root, &profile, DEFAULT_EMBED_MODEL, UNREACHABLE_DIM)?;

    let resp = call(
        &mut r,
        &mut w,
        3,
        side.reset,
        json!({ "yes": true, "resume": true }),
    )
    .await?;
    assert!(resp["error"].is_null(), "{}: {resp}", side.reset);
    assert_eq!(resp["result"]["resumed"], json!(true), "{resp}");
    assert_eq!(resp["result"]["intakes_reembedded"], json!(0), "{resp}");
    Ok(())
}

/// A declared dimension the backend does produce passes the check, under
/// a model the daemon was not serving: the handler resolves the new
/// model and the reset rebuilds the side under it.
async fn a_reset_whose_declared_dimension_matches_runs(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, side, &write_source(sources.path())?).await?;
    let profile = format!("{}-alternate", side.reset_run.replace('_', "-"));
    declare_profile(
        &root,
        &profile,
        EmbedStub::ALTERNATE_MODEL,
        EmbedStub::DIMENSION as u32,
    )?;

    let resp = call(&mut r, &mut w, 3, side.reset, json!({ "yes": true })).await?;
    assert!(resp["error"].is_null(), "{}: {resp}", side.reset);
    let after = side_state(&root, side).await?;
    assert_eq!(
        after.stamps[0].as_deref(),
        Some(EmbedStub::ALTERNATE_MODEL),
        "{}: {after:?}",
        side.reset
    );
    assert_eq!(
        after.stamps[1].as_deref(),
        Some(EmbedStub::DIMENSION.to_string().as_str()),
        "{}: {after:?}",
        side.reset
    );
    Ok(())
}

/// The check passes, the reset clears the side, and the model then goes
/// missing mid-pass. That failure is caller input too — `-32602` — and
/// it still carries the resume step: the code alone does not say whether
/// a reset changed anything, only the hint the handler writes does.
async fn a_model_missing_mid_pass_is_caller_input_and_still_resumable(
    side: Side,
    sock: PathBuf,
    root: PathBuf,
) -> Result<()> {
    let (mut r, mut w) = connect(&sock).await?;
    let sources = tempfile::tempdir()?;
    fill_one(&mut r, &mut w, side, &write_source(sources.path())?).await?;
    // The served model at the stub's width: the dimension probe is
    // answered from the cache the daemon filled, so the check passes and
    // the failure lands inside the pass.
    let profile = format!("{}-served", side.reset_run.replace('_', "-"));
    declare_profile(
        &root,
        &profile,
        DEFAULT_EMBED_MODEL,
        EmbedStub::DIMENSION as u32,
    )?;

    EmbedStub::set_failure(EmbedFailure::ModelNotFound);
    let err = error_of(&mut r, &mut w, 3, side.reset, json!({ "yes": true })).await;
    EmbedStub::set_failure(EmbedFailure::None);
    let err = err?;
    assert_eq!(err["code"].as_i64(), Some(-32602), "{}: {err}", side.reset);
    let hint = err["data"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("--resume"),
        "{}: the pass had started, so the resume step belongs in the hint: {err}",
        side.reset
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_against_an_unreachable_dimension_changes_nothing() -> Result<()> {
    with_daemon(|sock, root| {
        a_reset_against_an_unreachable_dimension_changes_nothing(BOOKS, sock, root)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_against_an_unreachable_dimension_changes_nothing() -> Result<()> {
    with_daemon(|sock, root| {
        a_reset_against_an_unreachable_dimension_changes_nothing(PAPERS, sock, root)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_resumed_reset_against_an_unreachable_dimension_is_refused() -> Result<()> {
    with_daemon(|sock, root| {
        a_resumed_reset_against_an_unreachable_dimension_is_refused(BOOKS, sock, root)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_resumed_reset_against_an_unreachable_dimension_is_refused() -> Result<()> {
    with_daemon(|sock, root| {
        a_resumed_reset_against_an_unreachable_dimension_is_refused(PAPERS, sock, root)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_resume_with_nothing_waiting_skips_the_check() -> Result<()> {
    with_daemon(|sock, root| a_resume_with_nothing_waiting_skips_the_check(BOOKS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_resume_with_nothing_waiting_skips_the_check() -> Result<()> {
    with_daemon(|sock, root| a_resume_with_nothing_waiting_skips_the_check(PAPERS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reset_whose_declared_dimension_matches_runs() -> Result<()> {
    with_daemon(|sock, root| a_reset_whose_declared_dimension_matches_runs(BOOKS, sock, root)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_reset_whose_declared_dimension_matches_runs() -> Result<()> {
    with_daemon(|sock, root| a_reset_whose_declared_dimension_matches_runs(PAPERS, sock, root))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_model_missing_mid_pass_is_caller_input_and_still_resumable() -> Result<()> {
    with_daemon(|sock, root| {
        a_model_missing_mid_pass_is_caller_input_and_still_resumable(BOOKS, sock, root)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn papers_vectors_model_missing_mid_pass_is_caller_input_and_still_resumable() -> Result<()> {
    with_daemon(|sock, root| {
        a_model_missing_mid_pass_is_caller_input_and_still_resumable(PAPERS, sock, root)
    })
    .await
}
