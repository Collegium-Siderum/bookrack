// SPDX-License-Identifier: Apache-2.0

//! Control-plane integration tests for the paper-side metadata write
//! surface: every method that addresses an intake must refuse an id
//! the paper catalog does not hold and must refuse it *before*
//! writing, and every method must go through the daemon's write path
//! rather than opening the catalog beside it.
//!
//! The paper override, review, and contributor tables carry no foreign
//! key onto `intakes`, so a write against a phantom id used to succeed
//! and leave a row nothing reads and `remove` never cascades away. The
//! error code alone does not pin that: the row check does.
//!
//! The embedder probe daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is
//! required.

#![cfg(unix)]

mod common;

use bookrack_catalog::Catalog;
use bookrack_core::ItemKind;
use eyre::{Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer, build_opts, connect, join_with_deadline, recv, send};
use bookrack_test_support::{ProcessEnv, process_env};

/// An id no intake in an empty library can carry.
const PHANTOM: i64 = 999_999;

/// Issue one request and return the whole response frame.
async fn call(
    reader: &mut Reader,
    w: &mut Writer,
    id: i64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    send(w, &serde_json::to_string(&req)?).await?;
    recv(reader).await
}

/// Assert one call was refused as caller input and named the id it
/// refused. Naming the id is what lets an operator tell "I typed the
/// wrong number" from "the server is broken".
fn assert_unknown_intake(resp: &Value, method: &str) {
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "{method} must refuse a phantom intake as caller input: {resp}"
    );
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("999999"),
        "{method} must name the id it refused: {resp}"
    );
    assert!(
        resp["result"].is_null(),
        "{method} must not also report success: {resp}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paper_metadata_writes_refuse_an_intake_the_catalog_does_not_hold() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    // The daemon owns this path; the assertion below reopens it after
    // shutdown rather than racing the daemon's own handle.
    let papers_catalog = data_root.path().join("papers_catalog.db");
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        // 1. `set` against a phantom id is caller input, not a fault.
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "papers.metadata.set",
            json!({"intake_id": PHANTOM, "field": "title", "value": "x"}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.set");

        // 2. The soft-report pair reports the refusal too, rather than
        //    `removed: false` / a success envelope.
        let resp = call(
            &mut reader,
            &mut w,
            2,
            "papers.metadata.clear",
            json!({"intake_id": PHANTOM, "field": "title"}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.clear");

        let resp = call(
            &mut reader,
            &mut w,
            3,
            "papers.metadata.void",
            json!({"intake_id": PHANTOM, "field": "title"}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.void");

        // 3. The four review verbs share one write path; each is
        //    dispatched separately, so each is asserted separately.
        // `ack` and `reject` demand a reason, so each verb is called
        // in the shape its parameters accept: a call refused for a
        // missing reason would never reach the intake check this test
        // is about.
        for (id, method, params) in [
            (
                4,
                "papers.metadata.ack",
                json!({"intake_id": PHANTOM, "reason": "acknowledged"}),
            ),
            (5, "papers.metadata.approve", json!({"intake_id": PHANTOM})),
            (
                6,
                "papers.metadata.reject",
                json!({"intake_id": PHANTOM, "reason": "rejected"}),
            ),
            (7, "papers.metadata.reopen", json!({"intake_id": PHANTOM})),
        ] {
            let resp = call(&mut reader, &mut w, id, method, params).await?;
            assert_unknown_intake(&resp, method);
        }

        // 4. `contributor_add` writes to a third table with the same
        //    missing foreign key.
        let resp = call(
            &mut reader,
            &mut w,
            8,
            "papers.metadata.contributor_add",
            json!({"intake_id": PHANTOM, "role": "author", "name": "x"}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.contributor_add");

        // 5. Regression guard: `reaudit` already reached -32602 through
        //    the typed glean error, and must keep doing so — the local
        //    guard is deliberately not on that path.
        let resp = call(
            &mut reader,
            &mut w,
            9,
            "papers.metadata.reaudit",
            json!({"intake_id": PHANTOM}),
        )
        .await?;
        assert_unknown_intake(&resp, "papers.metadata.reaudit");

        // 6. Regression guard: the book side answers the same input the
        //    same way. The two surfaces agreeing is the property that
        //    made this gap worth closing.
        let resp = call(
            &mut reader,
            &mut w,
            10,
            "metadata.set",
            json!({"book": PHANTOM, "field": "title", "value": "x"}),
        )
        .await?;
        assert_unknown_intake(&resp, "metadata.set");

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    // 7. The assertion the codes cannot make: nothing was written. A
    //    guard that returns -32602 and then falls through to the write
    //    would satisfy every check above and none of these.
    let catalog = Catalog::open(&papers_catalog)
        .map_err(|e| eyre!("reopen paper catalog at {}: {e}", papers_catalog.display()))?;
    assert!(
        catalog
            .overrides_for_address(PHANTOM, ItemKind::Paper)?
            .is_empty(),
        "a refused set/void must leave no override row behind"
    );
    assert!(
        catalog
            .contributors_for_address(PHANTOM, ItemKind::Paper)?
            .is_empty(),
        "a refused contributor_add must leave no contributor row behind"
    );
    assert!(
        catalog.review(PHANTOM, ItemKind::Paper)?.is_none(),
        "a refused review verb must leave no review row behind"
    );
    Ok(())
}

/// Seed one paper the re-audit path can actually run over: an intake,
/// its extraction envelope on disk, the base attrs glean derives from
/// that extraction, and the `node_paper_audit` row glean writes at
/// ingest time.
///
/// The audit projection is what `library.show_paper` reports, so a
/// fixture without it could not tell a re-audit that rewrites the row
/// from one that never wrote it.
fn seed_audited_paper(data_root: &std::path::Path) -> Result<i64> {
    use bookrack_catalog::{NewIntake, NewPublicationAttrs, NewReview, STATUS_PENDING};
    use bookrack_extract::envelope::{envelope_filename, write_envelope};
    use bookrack_extract::{
        Biblio, Block, BlockKind, Contributor, ContributorRole, CslType, Extraction, Provenance,
        TextLayerQuality, Toc,
    };
    use bookrack_glean::audit::{
        PaperAuditData, PaperAuditProfile, paper_report_to_audit_row, signals,
    };

    let biblio = Biblio {
        title: Some("Synthetic Findings in Test Spaces".to_string()),
        subtitle: None,
        publisher: None,
        year: Some(2019),
        year_raw: Some("2019".to_string()),
        isbn: None,
        series: None,
        language: Some("en".to_string()),
        contributors: vec![Contributor {
            name: "Alex Sample".to_string(),
            role: ContributorRole::Author,
            family: Some("Sample".to_string()),
            given: Some("Alex".to_string()),
            orcid: None,
        }],
        doi: Some("10.18653/v1/n19-1423".to_string()),
        arxiv_id: None,
        issn: None,
        container_title: Some("Journal of Synthetic Results".to_string()),
        abstract_text: Some(
            "A synthetic abstract long enough to clear the minimum length \
             the default profile requires for the abstract field."
                .to_string(),
        ),
        csl_type: Some(CslType::ArticleJournal),
    };
    let extraction = Extraction {
        biblio: biblio.clone(),
        blocks: vec![Block {
            kind: BlockKind::Body,
            text: "A synthetic body sample in English.".to_string(),
            source_unit: 0,
            style: None,
        }],
        toc: Toc::default(),
        provenance: Provenance {
            adapter: "pdf".to_string(),
            extractor_version: 1,
            text_layer_quality: TextLayerQuality::Usable,
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks: Vec::new(),
        },
    };

    let sha = "5eed5eed".to_string();
    let mut catalog = Catalog::open(&data_root.join("papers_catalog.db"))
        .map_err(|e| eyre!("open paper catalog to seed: {e}"))?;
    let intake_id = catalog
        .register_intake(ItemKind::Paper, &NewIntake::new(sha.clone()).format("pdf"))
        .map_err(|e| eyre!("seed intake: {e}"))?
        .into_intake()
        .intake_id;

    let papers_dir = data_root.join("papers");
    std::fs::create_dir_all(&papers_dir)?;
    let envelope_path = papers_dir.join(envelope_filename(ItemKind::Paper, intake_id));
    write_envelope(&envelope_path, &extraction, intake_id, &sha)
        .map_err(|e| eyre!("write envelope: {e}"))?;
    catalog
        .set_stored_path(
            ItemKind::Paper,
            intake_id,
            envelope_path.to_string_lossy().as_ref(),
        )
        .map_err(|e| eyre!("stored path: {e}"))?;

    let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Paper);
    attrs.title = biblio.title.clone();
    attrs.year = biblio.year.map(|y| y.to_string());
    attrs.doi = biblio.doi.clone();
    attrs.container_title = biblio.container_title.clone();
    attrs.abstract_text = biblio.abstract_text.clone();
    attrs.language = biblio.language.clone();
    attrs.csl_type = Some("article-journal".to_string());
    catalog
        .upsert_publication_attrs(&attrs)
        .map_err(|e| eyre!("attrs: {e}"))?;

    let effective = catalog
        .effective_publication_attrs(intake_id, ItemKind::Paper)
        .map_err(|e| eyre!("effective: {e}"))?;
    let profile = PaperAuditProfile::default_profile();
    let input = signals::PaperAuditInput {
        biblio: &extraction.biblio,
        provenance: &extraction.provenance,
        effective: &effective,
        body_sample: "A synthetic body sample in English.",
        source_stem: None,
    };
    let report = signals::audit_paper(&input, &profile, &PaperAuditData::default_data());
    let audited_at = catalog.now_iso().map_err(|e| eyre!("now: {e}"))?;
    let row = paper_report_to_audit_row(
        &report,
        intake_id,
        ItemKind::Paper.as_scope_str(),
        &profile,
        Some("article-journal"),
        &audited_at,
        "1",
        Some("glean_paper-2026-08-04T00:00:00Z-seed"),
    );
    catalog
        .upsert_node_paper_audit(&row)
        .map_err(|e| eyre!("seed audit row: {e}"))?;
    catalog
        .upsert_review(
            &NewReview::new(intake_id, ItemKind::Paper, "pipeline", STATUS_PENDING)
                .notes(report.to_json()),
        )
        .map_err(|e| eyre!("seed review: {e}"))?;

    // `show_paper` reads the corpus for the node shape alongside the
    // catalog, and opens it read-only, so the store has to exist.
    let mut corpus = bookrack_corpus::Corpus::open(&data_root.join("papers_corpus.db"))
        .map_err(|e| eyre!("open paper corpus to seed: {e}"))?;
    let partition = corpus
        .allocate_partition(intake_id)
        .map_err(|e| eyre!("allocate partition: {e}"))?;
    corpus
        .insert_node(
            &bookrack_corpus::NewNode::root(partition.book_root_id, bookrack_core::NodeType::Work)
                .title("Synthetic Findings in Test Spaces"),
        )
        .map_err(|e| eyre!("root node: {e}"))?;
    Ok(intake_id)
}

/// A re-audit through the control plane changes what `show_paper`
/// reports.
///
/// The verb wrote only the two-scalar rollup, and `show_paper` reads
/// its verdict off the audit projection — so a curator could correct a
/// field, re-audit, be told the verdict had changed, and see the same
/// judgement on every read surface. The two halves are asserted
/// together here because either one alone was already true.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reaudit_changes_what_show_paper_reports() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let before = call(
            &mut reader,
            &mut w,
            1,
            "library.show_paper",
            json!({ "intake_id": intake_id }),
        )
        .await?;
        assert_eq!(
            before["result"]["audit"]["verdict"].as_str(),
            Some("clean"),
            "the seeded paper must start clean or the flip proves nothing: {before}",
        );

        // Void the DOI: with no arXiv id and no ISSN either, the paper
        // is left with no stable identifier.
        let resp = call(
            &mut reader,
            &mut w,
            2,
            "papers.metadata.void",
            json!({"intake_id": intake_id, "field": "doi"}),
        )
        .await?;
        assert!(resp["error"].is_null(), "void failed: {resp}");

        let resp = call(
            &mut reader,
            &mut w,
            3,
            "papers.metadata.reaudit",
            json!({ "intake_id": intake_id }),
        )
        .await?;
        assert!(resp["error"].is_null(), "reaudit failed: {resp}");
        assert_eq!(
            resp["result"]["verdict"].as_str(),
            Some("needs_work"),
            "the re-audit must report the new judgement: {resp}",
        );

        let after = call(
            &mut reader,
            &mut w,
            4,
            "library.show_paper",
            json!({ "intake_id": intake_id }),
        )
        .await?;
        assert_eq!(
            after["result"]["audit"]["verdict"].as_str(),
            Some("needs_work"),
            "show_paper still reports the ingest-time judgement: {after}",
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

/// A paper-metadata curation write announces the library it changed.
///
/// Subscribers refresh their view of a library on `library.changed`,
/// and a write that opens the catalog beside the daemon's write path
/// publishes nothing: the desktop shell shows stale paper metadata
/// until something else happens to touch that library. The event is
/// also the observable end of the rest of the write path — the write
/// mutex against a concurrent ingest, and the MCP pause — so it is the
/// signal worth pinning.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_metadata_write_announces_the_library_it_changed() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;

    // Seed one real intake before bring-up: the event only fires on a
    // write that succeeds, and a phantom id is refused before any.
    let intake_id = {
        let mut catalog = Catalog::open(&data_root.path().join("papers_catalog.db"))
            .map_err(|e| eyre!("open paper catalog to seed: {e}"))?;
        catalog
            .register_intake(
                ItemKind::Paper,
                &bookrack_catalog::NewIntake::new("sha-announce").format("pdf"),
            )
            .map_err(|e| eyre!("seed intake: {e}"))?
            .into_intake()
            .intake_id
    };

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
        let _ = recv(&mut obs_reader).await?;
        // Drain the snapshot bundle a fresh subscriber receives, which
        // carries a `library.changed` of its own. `daemon.version` is
        // its last channel, so draining to it leaves only live
        // broadcasts without pinning the bundle's size.
        loop {
            let frame = recv(&mut obs_reader).await?;
            if frame["params"]["channel"].as_str() == Some("daemon.version") {
                break;
            }
        }

        let (mut wr_reader, mut wr_w) = connect(&sock).await?;
        let resp = call(
            &mut wr_reader,
            &mut wr_w,
            2,
            "papers.metadata.set",
            json!({"intake_id": intake_id, "field": "title", "value": "Announced"}),
        )
        .await?;
        assert!(
            resp["error"].is_null(),
            "the seeded intake must accept a set: {resp}"
        );

        let deadline = tokio::time::sleep(std::time::Duration::from_secs(20));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    panic!("no library.changed after papers.metadata.set")
                }
                frame = recv(&mut obs_reader) => {
                    if frame?["params"]["channel"].as_str() == Some("library.changed") {
                        break;
                    }
                }
            }
        }

        send(
            &mut wr_w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut wr_reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    // The write itself landed — an event published by a body that then
    // failed would satisfy the loop above and nothing else.
    let catalog = Catalog::open(&data_root.path().join("papers_catalog.db"))
        .map_err(|e| eyre!("reopen paper catalog: {e}"))?;
    assert!(
        !catalog
            .overrides_for_address(intake_id, ItemKind::Paper)?
            .is_empty(),
        "the announced write left no override row"
    );
    Ok(())
}

/// Every paper curation write appends one `metadata_audit` row, and the
/// row says what changed.
///
/// The paper side wrote the override and nothing else, so a curator
/// could rewrite a title and leave no record that anyone had. The
/// column assertions are what make this more than a row count: an
/// audit row that names the wrong table or drops the new value is a
/// trail nothing can be reconstructed from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_curation_write_records_an_audit_row() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "papers.metadata.set",
            json!({"intake_id": intake_id, "field": "title", "value": "A Curated Title"}),
        )
        .await?;
        assert!(resp["error"].is_null(), "the set must succeed: {resp}");
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    let catalog = Catalog::open(&data_root.path().join("papers_catalog.db"))
        .map_err(|e| eyre!("reopen paper catalog: {e}"))?;
    let node_id = bookrack_core::PartitionIdx::new(intake_id).root().get();
    let rows = catalog.metadata_audit_for_node(node_id)?;
    assert_eq!(
        rows.len(),
        1,
        "one paper curation write must leave exactly one audit row"
    );
    let row = &rows[0];
    assert_eq!(row.table_name, "node_publication_attrs");
    assert_eq!(row.action, "update");
    assert_eq!(row.field.as_deref(), Some("title"));
    assert_eq!(row.new_value.as_deref(), Some("A Curated Title"));
    assert_eq!(
        row.old_value.as_deref(),
        Some("Synthetic Findings in Test Spaces"),
        "the row must carry the value the edit replaced"
    );
    Ok(())
}

/// A second edit of the same field records the value the first one
/// left, so the trail reconstructs the sequence rather than only its
/// end state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_edit_records_the_value_the_first_one_left() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        for (id, value) in [(1, "First Correction"), (2, "Second Correction")] {
            let resp = call(
                &mut reader,
                &mut w,
                id,
                "papers.metadata.set",
                json!({"intake_id": intake_id, "field": "title", "value": value}),
            )
            .await?;
            assert!(resp["error"].is_null(), "set {value} must succeed: {resp}");
        }
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    let catalog = Catalog::open(&data_root.path().join("papers_catalog.db"))
        .map_err(|e| eyre!("reopen paper catalog: {e}"))?;
    let node_id = bookrack_core::PartitionIdx::new(intake_id).root().get();
    let rows = catalog.metadata_audit_for_node(node_id)?;
    assert_eq!(rows.len(), 2, "two edits must leave two audit rows");
    assert_eq!(
        rows[1].old_value.as_deref(),
        Some("First Correction"),
        "the second row must carry what the first edit left"
    );
    assert_eq!(rows[1].new_value.as_deref(), Some("Second Correction"));
    Ok(())
}

/// `contributor_remove` refuses a contributor row that belongs to
/// another paper, and leaves it in place.
///
/// The method addressed the row by its surrogate id alone, so any id
/// deleted any row — including one on a paper the caller never named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contributor_remove_refuses_a_row_on_another_paper() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let papers_catalog = data_root.path().join("papers_catalog.db");

    // Two papers, so "belongs to the named paper" has something to
    // fail against.
    let (paper_a, paper_b) = {
        let mut catalog =
            Catalog::open(&papers_catalog).map_err(|e| eyre!("open paper catalog to seed: {e}"))?;
        let mut register = |sha: &str| -> Result<i64> {
            Ok(catalog
                .register_intake(
                    ItemKind::Paper,
                    &bookrack_catalog::NewIntake::new(sha).format("pdf"),
                )
                .map_err(|e| eyre!("seed intake: {e}"))?
                .into_intake()
                .intake_id)
        };
        (register("sha-owner")?, register("sha-bystander")?)
    };

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "papers.metadata.contributor_add",
            json!({"intake_id": paper_a, "role": "author", "name": "Alex Sample"}),
        )
        .await?;
        assert!(
            resp["error"].is_null(),
            "contributor_add must succeed: {resp}"
        );
        let contributor_id = resp["result"]["contributor_id"]
            .as_i64()
            .ok_or_else(|| eyre!("contributor_add must report the new row's id: {resp}"))?;

        let resp = call(
            &mut reader,
            &mut w,
            2,
            "papers.metadata.contributor_remove",
            json!({"intake_id": paper_b, "contributor_id": contributor_id}),
        )
        .await?;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32602),
            "removing another paper's contributor must be refused as caller input: {resp}"
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

    let catalog = Catalog::open(&papers_catalog).map_err(|e| eyre!("reopen paper catalog: {e}"))?;
    assert_eq!(
        catalog
            .contributors_for_address(paper_a, ItemKind::Paper)?
            .len(),
        1,
        "a refused removal must leave the row it named in place"
    );
    Ok(())
}

/// A paper curation write is logged in `mcp_tool_calls` under the
/// control-plane method name, the way every paper *read* already is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_curation_write_is_logged_as_a_tool_call() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;
    // The tool-call log lives in the book-side catalog for every op,
    // paper reads included, and the recorder skips a data root that
    // holds no catalog at all. A library with both pipelines is what
    // this assertion is about, so the fixture materializes it.
    Catalog::open(&data_root.path().join("catalog.db"))
        .map_err(|e| eyre!("seed book catalog: {e}"))?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "papers.metadata.set",
            json!({"intake_id": intake_id, "field": "title", "value": "Logged"}),
        )
        .await?;
        assert!(resp["error"].is_null(), "the set must succeed: {resp}");
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    let catalog = Catalog::open(&data_root.path().join("catalog.db"))
        .map_err(|e| eyre!("reopen catalog: {e}"))?;
    let calls = catalog.tool_calls_for_tool("papers.metadata.set")?;
    assert_eq!(
        calls.len(),
        1,
        "a paper curation write must be logged like every other op"
    );
    assert_eq!(calls[0].status, "ok");
    Ok(())
}

/// A review verb leaves the ingest audit's report JSON in the review
/// row alone: the curator's words belong on the audit row, and the
/// report is the only copy of what the pipeline judged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_review_verb_does_not_overwrite_the_ingest_report() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;

    let seeded_notes = Catalog::open(&data_root.path().join("papers_catalog.db"))
        .map_err(|e| eyre!("open paper catalog: {e}"))?
        .review(intake_id, ItemKind::Paper)?
        .and_then(|r| r.notes)
        .ok_or_else(|| eyre!("the fixture must seed a report into the review row"))?;
    assert!(
        seeded_notes.contains("fields"),
        "the seeded notes must be the report JSON: {seeded_notes}"
    );

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "papers.metadata.approve",
            json!({"intake_id": intake_id, "reason": "checked against the published version"}),
        )
        .await?;
        assert!(resp["error"].is_null(), "approve must succeed: {resp}");
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;

    let catalog = Catalog::open(&data_root.path().join("papers_catalog.db"))
        .map_err(|e| eyre!("reopen paper catalog: {e}"))?;
    let review = catalog
        .review(intake_id, ItemKind::Paper)?
        .ok_or_else(|| eyre!("the review row must survive"))?;
    assert_eq!(review.status, "approved");
    assert_eq!(
        review.notes.as_deref(),
        Some(seeded_notes.as_str()),
        "the report JSON must survive a review verb"
    );

    // The words the curator supplied are on the audit row instead.
    let node_id = bookrack_core::PartitionIdx::new(intake_id).root().get();
    let rows = catalog.metadata_audit_for_node(node_id)?;
    assert_eq!(rows.len(), 1, "approve must append one audit row");
    assert_eq!(
        rows[0].reason.as_deref(),
        Some("checked against the published version")
    );
    Ok(())
}

/// The reason matrix the book side already enforces: `ack` and
/// `reject` must be justified, `approve` and `reopen` need not be.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_and_reject_require_a_reason_while_approve_and_reopen_do_not() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        for (id, method) in [(1, "papers.metadata.ack"), (2, "papers.metadata.reject")] {
            let resp = call(
                &mut reader,
                &mut w,
                id,
                method,
                json!({"intake_id": intake_id}),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(-32602),
                "{method} without a reason must be refused: {resp}"
            );
        }
        for (id, method) in [
            (3, "papers.metadata.approve"),
            (4, "papers.metadata.reopen"),
        ] {
            let resp = call(
                &mut reader,
                &mut w,
                id,
                method,
                json!({"intake_id": intake_id}),
            )
            .await?;
            assert!(
                resp["error"].is_null(),
                "{method} without a reason must succeed: {resp}"
            );
        }
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;
    Ok(())
}

/// A retired parameter is refused rather than silently dropped.
///
/// `serde` ignores unknown keys by default, so without
/// `deny_unknown_fields` a caller still passing `reviewer` or `notes`
/// is answered with a success envelope for a call that did something
/// else — the hardest failure shape for an operator to self-diagnose.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retired_paper_curation_parameter_is_refused() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        for (id, params) in [
            (1, json!({"intake_id": intake_id, "reviewer": "someone"})),
            (2, json!({"intake_id": intake_id, "notes": "free text"})),
        ] {
            let resp = call(
                &mut reader,
                &mut w,
                id,
                "papers.metadata.approve",
                params.clone(),
            )
            .await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(-32602),
                "a retired parameter must be refused, not ignored: {params} -> {resp}"
            );
        }
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await?;
    Ok(())
}

/// A paper re-audit is logged in `mcp_tool_calls` under its method
/// name, the way the other nine curation actions are.
///
/// It was the one action that never reached the ops layer, so the
/// call log had a hole exactly where the most expensive paper-side
/// write is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paper_reaudit_is_logged_as_a_tool_call() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;
    // The tool-call log lives in the book-side catalog, and the
    // recorder skips a data root that holds no catalog at all.
    Catalog::open(&data_root.path().join("catalog.db"))
        .map_err(|e| eyre!("seed book catalog: {e}"))?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "papers.metadata.reaudit",
            json!({"intake_id": intake_id}),
        )
        .await?;
        assert!(resp["error"].is_null(), "the re-audit must succeed: {resp}");
        assert!(
            resp["result"]["verdict"].is_string(),
            "the re-audit must still report its verdict: {resp}"
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

    let catalog = Catalog::open(&data_root.path().join("catalog.db"))
        .map_err(|e| eyre!("reopen catalog: {e}"))?;
    let calls = catalog.tool_calls_for_tool("papers.metadata.reaudit")?;
    assert_eq!(
        calls.len(),
        1,
        "a re-audit must be logged like every other op"
    );
    assert_eq!(calls[0].status, "ok");

    // A re-audit is a recomputation, not a curator edit, so it leaves
    // no `metadata_audit` row — the same split the book side keeps.
    let papers = Catalog::open(&data_root.path().join("papers_catalog.db"))
        .map_err(|e| eyre!("reopen paper catalog: {e}"))?;
    let node_id = bookrack_core::PartitionIdx::new(intake_id).root().get();
    assert!(
        papers.metadata_audit_for_node(node_id)?.is_empty(),
        "a re-audit must not be recorded as a curation edit"
    );
    Ok(())
}

/// The recomputed report follows an override while the stored
/// judgement stays where the last re-audit left it.
///
/// Both halves are asserted from one response: a read that recomputed
/// but reported the stored rollup, or one that echoed the stored row
/// as if it were fresh, would each satisfy only one of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_paper_report_recomputes_while_the_stored_judgement_stays_put() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        let before = call(
            &mut reader,
            &mut w,
            1,
            "library.show_paper_metadata_report",
            json!({"intake_id": intake_id}),
        )
        .await?;
        let before = &before["result"];
        assert!(
            !before.is_null(),
            "the seeded paper must have a report: {before}"
        );
        let title_origin = |r: &Value| -> String {
            r["fields"]
                .as_array()
                .expect("fields array")
                .iter()
                .find(|f| f["field"] == "title")
                .expect("a title row")["origin"]
                .as_str()
                .expect("origin string")
                .to_string()
        };
        assert_eq!(title_origin(before), "extracted");
        let stored_before = before["stored_verdict"].clone();
        assert!(
            stored_before.is_string(),
            "the fixture must have a stored judgement to compare against: {before}"
        );

        // Void the DOI: `article-journal` requires it, so the
        // recomputation must move off `clean` while the stored row,
        // which nothing re-audited, must not.
        let resp = call(
            &mut reader,
            &mut w,
            2,
            "papers.metadata.void",
            json!({"intake_id": intake_id, "field": "doi"}),
        )
        .await?;
        assert!(resp["error"].is_null(), "the void must succeed: {resp}");
        let resp = call(
            &mut reader,
            &mut w,
            3,
            "papers.metadata.set",
            json!({"intake_id": intake_id, "field": "title", "value": "A Curated Title"}),
        )
        .await?;
        assert!(resp["error"].is_null(), "the set must succeed: {resp}");

        let after = call(
            &mut reader,
            &mut w,
            4,
            "library.show_paper_metadata_report",
            json!({"intake_id": intake_id}),
        )
        .await?;
        let after = &after["result"];
        assert_eq!(
            title_origin(after),
            "override",
            "the recomputation must read the effective layer: {after}"
        );
        assert_eq!(
            after["stored_verdict"], stored_before,
            "nothing re-audited, so the stored judgement must not move: {after}"
        );
        assert_ne!(
            after["verdict"], after["stored_verdict"],
            "the edit must show up as a divergence between the two: {after}"
        );

        // A re-audit is what closes the gap.
        let resp = call(
            &mut reader,
            &mut w,
            5,
            "papers.metadata.reaudit",
            json!({"intake_id": intake_id}),
        )
        .await?;
        assert!(resp["error"].is_null(), "the re-audit must succeed: {resp}");
        let settled = call(
            &mut reader,
            &mut w,
            6,
            "library.show_paper_metadata_report",
            json!({"intake_id": intake_id}),
        )
        .await?;
        let settled = &settled["result"];
        assert_eq!(
            settled["verdict"], settled["stored_verdict"],
            "after a re-audit the two must agree: {settled}"
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
    Ok(())
}

/// The paper audit trail reports the rows the curation writes left,
/// attributed to the surface that made them.
///
/// This is what welds the read to the write: if the write side stopped
/// recording, this read would have nothing to return.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_paper_audit_trail_reports_what_the_curation_writes_recorded() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let intake_id = seed_audited_paper(data_root.path())?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let resp = call(
            &mut reader,
            &mut w,
            1,
            "papers.metadata.set",
            json!({
                "intake_id": intake_id,
                "field": "title",
                "value": "A Curated Title",
                "reason": "checked against the published version",
            }),
        )
        .await?;
        assert!(resp["error"].is_null(), "the set must succeed: {resp}");

        let trail = call(
            &mut reader,
            &mut w,
            2,
            "library.show_paper_audit_trail",
            json!({"intake_id": intake_id}),
        )
        .await?;
        let rows = trail["result"]
            .as_array()
            .ok_or_else(|| eyre!("the trail must be an array: {trail}"))?;
        assert_eq!(rows.len(), 1, "one edit, one row: {trail}");
        assert_eq!(rows[0]["field"], "title");
        assert_eq!(rows[0]["new_value"], "A Curated Title");
        assert_eq!(rows[0]["old_value"], "Synthetic Findings in Test Spaces");
        assert_eq!(rows[0]["reason"], "checked against the published version");
        assert_eq!(rows[0]["actor_kind"], "human");

        // The book-side trail must not answer for a paper id: the two
        // catalogs number independently, and one answering for the
        // other is how a curator reads the wrong history.
        let book_trail = call(
            &mut reader,
            &mut w,
            3,
            "library.show_audit_trail",
            json!({"intake_id": intake_id}),
        )
        .await?;
        assert!(
            book_trail["result"].is_null(),
            "the book trail must not report a paper's edits: {book_trail}"
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
    Ok(())
}

/// Register one paper carrying an audit grade and a review status, so
/// a listing has something to include and something to leave out.
fn seed_graded_paper(
    data_root: &std::path::Path,
    sha: &str,
    title: &str,
    confidence: &str,
    status: &str,
) -> Result<i64> {
    use bookrack_catalog::{NewIntake, NewPublicationAttrs, NewReview};

    let mut catalog = Catalog::open(&data_root.join("papers_catalog.db"))
        .map_err(|e| eyre!("open paper catalog to seed: {e}"))?;
    let intake_id = catalog
        .register_intake(ItemKind::Paper, &NewIntake::new(sha).format("pdf"))
        .map_err(|e| eyre!("seed intake: {e}"))?
        .into_intake()
        .intake_id;
    let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Paper);
    attrs.title = Some(title.to_string());
    attrs.confidence = Some(confidence.to_string());
    attrs.audit_verdict = Some(if confidence == "high" {
        "clean".to_string()
    } else {
        "needs_work".to_string()
    });
    catalog
        .upsert_publication_attrs(&attrs)
        .map_err(|e| eyre!("seed attrs: {e}"))?;
    catalog
        .upsert_review(&NewReview::new(
            intake_id,
            ItemKind::Paper,
            "pipeline",
            status,
        ))
        .map_err(|e| eyre!("seed review: {e}"))?;
    Ok(intake_id)
}

/// The paper review queue holds the papers that need review and only
/// those, and a curator's decision takes one off it.
///
/// Three papers are seeded, one per review state the preset spans plus
/// one it must exclude: a fixture missing `acknowledged` passes
/// against a preset that only looks for `pending`, and a fixture with
/// one row passes whether or not the predicate was ever bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_paper_review_queue_holds_what_needs_review_and_empties_on_a_decision() -> Result<()> {
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let flagged = seed_graded_paper(
        data_root.path(),
        "sha-flagged",
        "A Flagged Paper",
        "low",
        "pending",
    )?;
    let acknowledged = seed_graded_paper(
        data_root.path(),
        "sha-acknowledged",
        "An Acknowledged Paper",
        "medium",
        "acknowledged",
    )?;
    let settled = seed_graded_paper(
        data_root.path(),
        "sha-settled",
        "A Settled Paper",
        "high",
        "approved",
    )?;

    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        true,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;
        let rows = |resp: &Value| -> Vec<Value> {
            resp["result"]["rows"]
                .as_array()
                .expect("rows array")
                .clone()
        };
        let ids = |resp: &Value| -> Vec<i64> {
            let mut v: Vec<i64> = rows(resp)
                .iter()
                .map(|r| r["intake_id"].as_i64().expect("intake_id"))
                .collect();
            v.sort_unstable();
            v
        };

        let all = call(
            &mut reader,
            &mut w,
            1,
            "library.list_paper_metadata",
            json!({}),
        )
        .await?;
        assert_eq!(
            ids(&all),
            vec![flagged, acknowledged, settled],
            "the unfiltered listing must hold every paper: {all}"
        );
        // Each row's projection has to come off the paper scope. With
        // no predicate to bind, the id list alone is the same whichever
        // scope the query joined on, so the row content is what says
        // which one it read.
        let flagged_row = rows(&all)
            .into_iter()
            .find(|r| r["intake_id"].as_i64() == Some(flagged))
            .ok_or_else(|| eyre!("the flagged paper must be listed: {all}"))?;
        assert_eq!(flagged_row["title"], "A Flagged Paper", "{flagged_row}");
        assert_eq!(flagged_row["confidence"], "low", "{flagged_row}");
        assert_eq!(flagged_row["review_status"], "pending", "{flagged_row}");

        let queue = call(
            &mut reader,
            &mut w,
            2,
            "library.list_paper_pending_reviews",
            json!({}),
        )
        .await?;
        assert_eq!(
            ids(&queue),
            vec![flagged, acknowledged],
            "the queue spans both unfinished review states and excludes the settled one: {queue}"
        );

        // Deciding takes it off the queue — the loop the paper side
        // never closed.
        let resp = call(
            &mut reader,
            &mut w,
            3,
            "papers.metadata.approve",
            json!({"intake_id": flagged, "reason": "checked against the venue"}),
        )
        .await?;
        assert!(resp["error"].is_null(), "approve must succeed: {resp}");

        let queue = call(
            &mut reader,
            &mut w,
            4,
            "library.list_paper_pending_reviews",
            json!({}),
        )
        .await?;
        assert_eq!(
            ids(&queue),
            vec![acknowledged],
            "an approved paper must leave the queue and the others must stay: {queue}"
        );
        let all = call(
            &mut reader,
            &mut w,
            5,
            "library.list_paper_metadata",
            json!({}),
        )
        .await?;
        assert_eq!(
            ids(&all).len(),
            3,
            "leaving the queue is not leaving the registry: {all}"
        );

        // Paging reports the whole result set, not the page.
        let page = call(
            &mut reader,
            &mut w,
            6,
            "library.list_paper_metadata",
            json!({"limit": 1}),
        )
        .await?;
        assert_eq!(ids(&page).len(), 1, "limit must bound the page: {page}");
        assert_eq!(page["result"]["total"].as_u64(), Some(3), "{page}");
        assert_eq!(page["result"]["truncated"].as_bool(), Some(true), "{page}");

        // The filtered listing is the one with a predicate to bind, so
        // it is the one that fails outright on the wrong scope.
        let filtered = call(
            &mut reader,
            &mut w,
            7,
            "library.list_paper_metadata",
            json!({"title_substring": "Flagged"}),
        )
        .await?;
        assert_eq!(
            ids(&filtered),
            vec![flagged],
            "the title filter must read the paper scope: {filtered}"
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
    Ok(())
}
