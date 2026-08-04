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
        for (id, method) in [
            (4, "papers.metadata.ack"),
            (5, "papers.metadata.approve"),
            (6, "papers.metadata.reject"),
            (7, "papers.metadata.reopen"),
        ] {
            let resp = call(
                &mut reader,
                &mut w,
                id,
                method,
                json!({"intake_id": PHANTOM}),
            )
            .await?;
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
