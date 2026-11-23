// SPDX-License-Identifier: Apache-2.0

//! The library methods the control plane serves from the ops method
//! table, driven over the socket: the reference book's overlay edit,
//! which the table brings to this surface, and the book report, which
//! grades under the library's own audit overlays as a re-audit does.
//!
//! The embedder probe bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

use std::future::Future;
use std::path::{Path, PathBuf};

use bookrack_catalog::{Catalog, NewIntake, NewOverride};
use bookrack_core::ItemKind;
use bookrack_extract::{
    Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc, envelope_filename,
    write_envelope,
};
use bookrack_refs::{NewBook, NewEntry, Refs};
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::{Result, eyre};
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

/// Bring a daemon up on a root `seed` prepared, run `body` against its
/// socket and root, then shut it down.
async fn with_seeded_daemon<T, S, F, Fut>(seed: S, body: F) -> Result<()>
where
    T: Send + 'static,
    S: FnOnce(&Path) -> Result<T>,
    F: FnOnce(PathBuf, PathBuf, T) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    process_env(ProcessEnv::daemon());
    let data_root = tempfile::tempdir()?;
    let runtime_root = tempfile::tempdir()?;
    let seeded = seed(data_root.path())?;
    let runtime = bookrack_runtime::DaemonRuntime::start(build_opts(
        data_root.path().into(),
        runtime_root.path().into(),
        false,
    ))
    .await?;
    let sock = runtime.control_sock.path.clone();
    let root = data_root.path().to_path_buf();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let driver = tokio::spawn(async move {
        let outcome = body(sock.clone(), root, seeded).await;
        let (_, mut w) = connect(&sock).await?;
        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        outcome
    });
    join_with_deadline(runtime, repl_handle, driver).await
}

/// The name the daemon serves its one library under.
async fn served_name(reader: &mut Reader, w: &mut Writer) -> Result<String> {
    let rows = ok_result(reader, w, 1, "library.list", json!({})).await?;
    rows[0]["name"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| eyre!("library.list names no library: {rows}"))
}

/// One reference book with one entry, written before bring-up.
fn seed_reference(root: &Path) -> Result<()> {
    let refs = Refs::open_for_write(&root.join("reference.db"), &root.join("backup"))?;
    refs.upsert_book(&NewBook {
        book_slug: "book_a".to_owned(),
        schema_name: "name_translation".to_owned(),
        schema_version: 1,
        parser_version: "0.1.0".to_owned(),
        title_zh: "book a".to_owned(),
        title_en: None,
        edition: None,
        publisher: None,
        year: None,
        isbn: None,
        authority_rank: 10,
        built_at: "2026-06-25T00:00:00Z".to_owned(),
        intake_id: None,
    })?;
    refs.upsert_entry(&NewEntry {
        book_slug: "book_a".to_owned(),
        entry_key: "ashgrovewren".to_owned(),
        headword: "Ashgrove, Wren".to_owned(),
        aliases: vec![],
        payload: json!({ "country": "UK" }),
        fts_text: "Ashgrove, Wren".to_owned(),
        source: json!({ "book_slug": "book_a", "page": 1, "sheet": 1 }),
        quality_flags: vec![],
    })?;
    Ok(())
}

/// The property-catalog key the overlay test edits.
const OVERLAY_KEY: &str = "country";

/// `reference.overlay_set` is a control-plane method: an edit lands and
/// the next lookup reads it, an overlay key outside the property
/// catalog is caller input, and the call names its library because a
/// write is never routed to whichever library the default happens to
/// be.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reference_overlay_set_over_control_plane() -> Result<()> {
    with_seeded_daemon(seed_reference, |sock, _root, ()| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let library = served_name(&mut r, &mut w).await?;

        let receipt = ok_result(
            &mut r,
            &mut w,
            2,
            "reference.overlay_set",
            json!({
                "library": library,
                "book_slug": "book_a",
                "entry_key": "ashgrovewren",
                "overlay": { OVERLAY_KEY: "GB" },
                "reason": "the second edition",
            }),
        )
        .await?;
        assert_eq!(receipt["book_slug"], "book_a", "{receipt}");
        assert_eq!(receipt["entry_key"], "ashgrovewren", "{receipt}");
        assert!(receipt["edited_at"].is_string(), "{receipt}");

        let found = ok_result(
            &mut r,
            &mut w,
            3,
            "reference.lookup",
            json!({ "book": "book_a", "entry_key": "ashgrovewren" }),
        )
        .await?;
        let hit = &found["hits"][0];
        assert_eq!(hit["has_overlay"], true, "{found}");
        assert_eq!(hit["edit_reason"], "the second edition", "{found}");
        assert_eq!(hit["payload"][OVERLAY_KEY], "GB", "{found}");

        let refused = call(
            &mut r,
            &mut w,
            4,
            "reference.overlay_set",
            json!({
                "library": library,
                "book_slug": "book_a",
                "entry_key": "ashgrovewren",
                "overlay": { "no_such_property": 1 },
                "reason": "r",
            }),
        )
        .await?;
        assert_eq!(refused["error"]["code"], -32602, "{refused}");

        let unnamed = call(
            &mut r,
            &mut w,
            5,
            "reference.overlay_set",
            json!({
                "book_slug": "book_a",
                "entry_key": "ashgrovewren",
                "overlay": { OVERLAY_KEY: "IE" },
                "reason": "r",
            }),
        )
        .await?;
        assert_eq!(unnamed["error"]["code"], -32602, "{unnamed}");
        let message = unnamed["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("library"), "{unnamed}");
        Ok(())
    })
    .await
}

fn sample_extraction() -> Extraction {
    Extraction {
        blocks: vec![
            Block {
                kind: BlockKind::Heading { level: 1 },
                text: "Chapter One".into(),
                source_unit: 0,
                style: None,
            },
            Block {
                kind: BlockKind::Body,
                text: "Some sample prose for the audit body sample.".into(),
                source_unit: 0,
                style: None,
            },
        ],
        toc: Toc::default(),
        biblio: Biblio::default(),
        provenance: Provenance {
            adapter: "txt".into(),
            extractor_version: 1,
            text_layer_quality: TextLayerQuality::BornDigital,
            skipped_units: vec![],
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks: Vec::new(),
            rule: None,
        },
    }
}

/// A book with a cached extraction whose title is overridden to the
/// placeholder `Untitled`.
fn seed_placeholder_book(root: &Path) -> Result<i64> {
    let books = root.join("books");
    std::fs::create_dir_all(&books)?;
    let mut catalog = Catalog::open_for_write_unbacked(&root.join("catalog.db"))?;
    let intake_id = catalog
        .register_intake(
            ItemKind::Book,
            &NewIntake::new("sha-report").format("txt").byte_size(1),
        )?
        .into_intake()
        .intake_id;
    let path = books.join(envelope_filename(ItemKind::Book, intake_id));
    write_envelope(&path, &sample_extraction(), intake_id, "sha-report")?;
    catalog.set_stored_path(ItemKind::Book, intake_id, &path.to_string_lossy())?;
    catalog.set_override(&NewOverride::new(
        intake_id,
        ItemKind::Book,
        "title",
        Some("Untitled".to_owned()),
        "human",
    ))?;
    Ok(intake_id)
}

/// Whether the report's `title` row carries the placeholder flag.
fn title_is_flagged(report: &Value) -> bool {
    report["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["field"] == "title")
        .and_then(|row| row["flags"].as_array())
        .is_some_and(|flags| flags.iter().any(|flag| flag == "placeholder_value"))
}

/// The control plane's book report grades under the library's audit
/// profile overlay, read when the call arrives: the rules a re-audit of
/// the book would store under.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_book_report_grades_under_the_librarys_profile_overlay() -> Result<()> {
    with_seeded_daemon(seed_placeholder_book, |sock, root, book| async move {
        let (mut r, mut w) = connect(&sock).await?;
        let shipped = ok_result(
            &mut r,
            &mut w,
            2,
            "library.show_metadata_report",
            json!({ "intake_id": book }),
        )
        .await?;
        assert!(
            title_is_flagged(&shipped),
            "the shipped profile flags a placeholder title: {shipped}"
        );

        let rules = root.join("audit-rules");
        std::fs::create_dir_all(&rules)?;
        std::fs::write(
            rules.join("audit_profile.local.toml"),
            "schema_version = 1\n\n[title]\nplaceholder_check = false\n",
        )?;
        let overlaid = ok_result(
            &mut r,
            &mut w,
            3,
            "library.show_metadata_report",
            json!({ "intake_id": book }),
        )
        .await?;
        assert!(
            !title_is_flagged(&overlaid),
            "the overlay turns the placeholder check off: {overlaid}"
        );
        Ok(())
    })
    .await
}
