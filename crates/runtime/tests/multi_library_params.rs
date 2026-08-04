// SPDX-License-Identifier: Apache-2.0

//! Ingest parameters follow the job's target library, not the
//! bring-up-selected one.
//!
//! Two libraries, both agreeing on the reranker stage so bring-up
//! proceeds, disagreeing on everything a job carries: `alpha` (the
//! registry default, and so the primary) declares the stub's alternate
//! embedding model and an `audit-rules/` overlay that switches the
//! metadata audit off; `beta` declares neither. A book submitted to
//! `beta` must be ingested under `beta`'s model and `beta`'s overlay.
//!
//! Both models are answered by `bookrack_test_support::EmbedStub`, so
//! no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use bookrack_config::{
    DEFAULT_EMBED_MODEL, LibraryKind, ManifestIdentitySeed, set_manifest_index_profile,
};
use bookrack_test_support::{EmbedStub, ProcessEnv, Sandbox, process_env};
use eyre::{ContextCompat, Result};
use serde_json::{Value, json};

use crate::common::{Reader, connect, join_with_deadline, recv, send};

/// A synthetic text long enough for the pipeline to extract prose,
/// plan chunks, and embed them against the stub.
const BOOK_TEXT: &str = "\
Chapter One

The synthetic narrator opened the synthetic ledger and began to
count. Every entry in the ledger described a fictional object, and
every fictional object had a fictional weight. Counting the weights
took the narrator most of the morning.

Chapter Two

On the second day the ledger grew a second column. The new column
held colours instead of weights, and the narrator sorted them from
dull to bright. Sorting colours was slower than counting weights.
";

/// A profile declaring `model`, with no reranker stage — so the two
/// libraries agree on the section bring-up serves once for the set.
fn profile_toml(name: &str, model: &str) -> String {
    format!(
        r#"schema_version = 1
name = "{name}"
description = "Test profile."

[embed]
backend = "ollama"
model = "{model}"
dim = 8

[ann]
kind = "ivf-pq"
num_partitions = 256
num_sub_vectors = 128
num_bits = 8
nprobes = 16
refine_factor = 10

[reranker]
kind = "none"
"#
    )
}

/// An overlay that switches the metadata audit off. Ingest records the
/// skip in the review row's notes, which is what makes "whose overlay
/// was read" observable from the catalog.
const AUDIT_OFF_OVERLAY: &str = "schema_version = 1\naudit_enabled = false\n";

/// Seed a two-library registry whose members differ in embedding model
/// and audit overlay: `alpha` is the registry default and carries both
/// deviations, `beta` carries neither.
fn world() -> &'static Sandbox {
    static SEEDED: OnceLock<()> = OnceLock::new();
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    SEEDED.get_or_init(|| {
        let profiles = sandbox.path().join("index-profiles");
        std::fs::create_dir_all(&profiles).expect("profile directory");
        std::fs::write(
            profiles.join("alt-model.toml"),
            profile_toml("alt-model", EmbedStub::ALTERNATE_MODEL),
        )
        .expect("write alpha's profile");

        let alpha = sandbox.data_root("alpha-root");
        let beta = sandbox.data_root("beta-root");
        sandbox.write_registry_entries(
            Some("alpha"),
            &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
        );

        // Only alpha deviates: its own model, and its own audit rules.
        set_manifest_index_profile(
            alpha.as_path(),
            Some("alt-model"),
            ManifestIdentitySeed {
                name: "alpha",
                kind: LibraryKind::Test,
                description: None,
            },
        )
        .expect("declare alpha's index profile");
        let alpha_rules = alpha.join("audit-rules");
        std::fs::create_dir_all(&alpha_rules).expect("alpha audit-rules");
        std::fs::write(
            alpha_rules.join("audit_profile.local.toml"),
            AUDIT_OFF_OVERLAY,
        )
        .expect("write alpha's audit overlay");
    });
    sandbox
}

/// Read frames until the job's closing `queue.tick`, returning its
/// `last_finished` summary.
async fn drain_until_done(reader: &mut Reader, job_id: &str, timeout: Duration) -> Result<Value> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Err(eyre::eyre!("timed out waiting for job {job_id}")),
            frame = recv(reader) => {
                let frame = frame?;
                let value = &frame["params"]["value"];
                if frame["params"]["channel"].as_str() == Some("queue.tick")
                    && value["last_finished"]["job_id"].as_str() == Some(job_id)
                {
                    return Ok(value["last_finished"].clone());
                }
            }
        }
    }
}

/// A book submitted to `beta` is ingested under `beta`'s declarations.
///
/// Two independent assertions on one ingest, because the two follow
/// from one defect — the parameters a job runs under came from the
/// primary's configuration:
///
/// - the index stamp written into `beta`'s corpus names `beta`'s
///   embedding model, not `alpha`'s. This is the destructive half: the
///   dimension beside it comes from `beta`'s real embedder, so a stamp
///   taken from `alpha` is internally inconsistent and every later
///   check trusts it;
/// - the metadata audit runs, because `beta` declares no overlay
///   switching it off. `alpha` does, and `docs/configuration.md`
///   promises the overlay is read per data root.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_targeting_a_library_runs_under_that_librarys_parameters() -> Result<()> {
    let sandbox = world();
    let runtime_root = tempfile::tempdir()?;
    let book_dir = tempfile::tempdir()?;
    let book_path = book_dir.path().join("synthetic-ledger.txt");
    std::fs::write(&book_path, BOOK_TEXT)?;
    let beta_root = sandbox.data_root("beta-root");

    let mut opts = bookrack_runtime::RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = bookrack_runtime::DaemonRuntime::start(opts).await?;
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

        let (mut wr_reader, mut wr_w) = connect(&sock).await?;
        let submit = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "ingest.submit",
            "params": {"paths": [book_path], "library": "beta"},
        });
        send(&mut wr_w, &submit.to_string()).await?;
        let submit_resp = recv(&mut wr_reader).await?;
        let job_id = submit_resp["result"]["job_ids"][0]
            .as_str()
            .with_context(|| format!("missing job id: {submit_resp}"))?
            .to_string();

        let last_finished =
            drain_until_done(&mut obs_reader, &job_id, Duration::from_secs(60)).await?;
        assert_eq!(
            last_finished["state"].as_str(),
            Some("done"),
            "job did not finish clean: {last_finished}"
        );

        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut wr_reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    // Path 4: the stamp in beta's corpus names beta's model.
    let stamps = bookrack_runtime::profile::built_stamps(&beta_root.join("corpus.db"))
        .map_err(|e| eyre::eyre!("read beta's index stamps: {e}"))?
        .with_context(|| "beta's corpus carries no index stamps after an ingest")?;
    let (model, _dim) = stamps
        .embed_pair()
        .with_context(|| "beta's corpus carries no embed stamp after an ingest")?;
    assert_eq!(
        model, DEFAULT_EMBED_MODEL,
        "beta's index stamp must name beta's own embedding model, not the primary's",
    );

    // Paths 2/3: the audit ran, because beta declares no overlay
    // switching it off — alpha does.
    let catalog = bookrack_catalog::Catalog::open(&beta_root.join("catalog.db"))?;
    let review = catalog
        .review(1, bookrack_core::ItemKind::Book)?
        .with_context(|| "beta's catalog holds no review row for the ingested book")?;
    let notes = review.notes.unwrap_or_default();
    assert!(
        !notes.contains("audit skipped"),
        "beta was ingested under alpha's audit overlay: {notes:?}",
    );
    Ok(())
}

/// `library.info { name }` answers with that library's identity, not
/// the primary's.
///
/// The counts on the card already come from the named library's
/// handle; the static half — root, name, and configured embedding
/// model — came from a snapshot taken at bring-up from the
/// bring-up-selected library. The card is where an operator confirms
/// which library they are addressing, so a card mixing one library's
/// counts with another's identity is the one that cannot be allowed to
/// disagree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_card_reports_the_named_librarys_identity() -> Result<()> {
    let sandbox = world();
    let runtime_root = tempfile::tempdir()?;
    let beta_root = sandbox.data_root("beta-root");

    let mut opts = bookrack_runtime::RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = bookrack_runtime::DaemonRuntime::start(opts).await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":1,"method":"library.info","params":{"name":"beta"}}"#,
        )
        .await?;
        let resp = recv(&mut reader).await?;
        let card = &resp["result"];

        assert_eq!(
            card["library_name"].as_str(),
            Some("beta"),
            "the card names another library: {resp}"
        );
        assert_eq!(
            card["data_dir"].as_str(),
            Some(beta_root.display().to_string().as_str()),
            "the card reports another library's root: {resp}"
        );
        assert_eq!(
            card["embed_model_configured"].as_str(),
            Some(DEFAULT_EMBED_MODEL),
            "the card reports another library's embedding model: {resp}"
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

/// A write method acts on the library its `library` parameter names.
///
/// `dryrun` is the cheapest witness: it writes its JSONL and summary
/// under `<data_root>/dryruns/`, so which root grew the directory says
/// which library the call reached. Without the parameter the method
/// had no way to be told, and every write method acted on the
/// bring-up-selected library whatever the caller meant.
///
/// The second half is the same question from the other side: a library
/// name the registry does not know must be refused as caller input,
/// not silently ignored on the way to the primary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_method_acts_on_the_library_its_parameter_names() -> Result<()> {
    let sandbox = world();
    let runtime_root = tempfile::tempdir()?;
    let book_dir = tempfile::tempdir()?;
    let book_path = book_dir.path().join("synthetic-ledger.txt");
    std::fs::write(&book_path, BOOK_TEXT)?;
    let alpha_root = sandbox.data_root("alpha-root");
    let beta_root = sandbox.data_root("beta-root");

    let mut opts = bookrack_runtime::RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    // `dryrun` is queue-bound; a headless daemon without a worker
    // short-circuits it before the handler runs.
    opts.spawn_queue_worker = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = bookrack_runtime::DaemonRuntime::start(opts).await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "dryrun",
            "params": {"path": book_path, "library": "beta"},
        });
        send(&mut w, &req.to_string()).await?;
        let resp = recv(&mut reader).await?;
        assert!(
            resp["error"].is_null(),
            "dryrun against beta failed: {resp}"
        );

        let unknown = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "dryrun",
            "params": {"path": book_path, "library": "no-such-library"},
        });
        send(&mut w, &unknown.to_string()).await?;
        let resp = recv(&mut reader).await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32010),
            "an unknown library must be refused, not routed to the primary: {resp}"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    assert!(
        beta_root.join("dryruns").exists(),
        "the dry run did not land in the library it named",
    );
    assert!(
        !alpha_root.join("dryruns").exists(),
        "the dry run landed in the primary's root instead of the named library's",
    );
    Ok(())
}
