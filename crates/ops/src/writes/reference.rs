// SPDX-License-Identifier: Apache-2.0

//! Reference-book overlay writes.
//!
//! An overlay layers a curator's edit on top of one distilled entry of a
//! reference book, without touching the distilled row. The payload's
//! keys are judged against the property catalog before the store is
//! opened, so a refused call leaves no store behind; the write runs in
//! the library's write session, which records the call and announces
//! the library once the overlay is on disk.

use bookrack_distill::Catalogs;
use bookrack_embed::Embedder;
use bookrack_refs::{NewOverlay, Refs};
use serde::{Deserialize, Serialize};

use crate::reads::reference::{ReferenceError, catalogs};
use crate::recorder::Recorder;
use crate::write_session::{ChangeSet, WriteSession};

/// The public name the overlay write is recorded and advertised under.
pub const OVERLAY_SET_TOOL: &str = "reference.overlay_set";

/// One overlay edit: the entry it lands on, the payload, and why.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReferenceOverlaySetRequest {
    /// The reference book the entry belongs to.
    pub book_slug: String,
    /// The entry's key within that book.
    pub entry_key: String,
    /// A JSON object whose keys are property-catalog keys.
    pub overlay: serde_json::Value,
    /// Free-text edit summary recorded on the overlay row.
    pub reason: String,
}

/// The receipt of a written overlay.
#[derive(Debug, Clone, Serialize)]
pub struct ReferenceOverlaySetResult {
    /// The reference book the entry belongs to.
    pub book_slug: String,
    /// The entry's key within that book.
    pub entry_key: String,
    /// When the overlay was written, RFC 3339 UTC.
    pub edited_at: String,
}

/// Check an overlay payload against the property catalog: it is a JSON
/// object and every key of it is a catalog key. Reads no store, so a
/// front end can refuse a malformed edit before it opens a session.
pub fn validate_overlay(
    catalogs: &Catalogs,
    request: &ReferenceOverlaySetRequest,
) -> Result<(), ReferenceError> {
    let obj = request.overlay.as_object().ok_or_else(|| {
        ReferenceError::InvalidArgument(
            "overlay must be a JSON object whose keys are property catalog keys".to_string(),
        )
    })?;
    for key in obj.keys() {
        if !catalogs.properties.entries.contains_key(key) {
            return Err(ReferenceError::UnknownOverlayProperty { key: key.clone() });
        }
    }
    Ok(())
}

/// Write `request`'s overlay into the session's library.
///
/// Judges the payload first, then opens the reference store through its
/// write door — which creates a missing store, snapshots and migrates
/// one this build is ahead of, and refuses one it cannot serve — and
/// upserts the row, stamped with the current time. The call is recorded
/// under [`OVERLAY_SET_TOOL`], attributed to the session's caller, and
/// the change set announces the session's library.
pub fn set_overlay<E: Embedder>(
    session: &mut WriteSession<E>,
    request: &ReferenceOverlaySetRequest,
) -> Result<(ReferenceOverlaySetResult, ChangeSet), ReferenceError> {
    let ops = session.ops();
    let recorder = Recorder::start_with_caller(
        ops.catalog_db(),
        session.caller(),
        OVERLAY_SET_TOOL,
        serde_json::json!({
            "book_slug": request.book_slug,
            "entry_key": request.entry_key,
        }),
    );
    let result = write_overlay(ops.reference_db_path(), ops.backup_dir(), request);
    recorder.finish_with_kind(&result, error_kind);
    let receipt = result?;
    Ok((
        receipt,
        ChangeSet::announced(session.intent().library.clone()),
    ))
}

fn write_overlay(
    path: std::path::PathBuf,
    backup_dir: &std::path::Path,
    request: &ReferenceOverlaySetRequest,
) -> Result<ReferenceOverlaySetResult, ReferenceError> {
    validate_overlay(catalogs()?, request)?;
    let refs = Refs::open_for_write(&path, backup_dir)
        .map_err(|source| ReferenceError::Open { path, source })?;
    let edited_at = chrono::Utc::now().to_rfc3339();
    refs.upsert_overlay(&NewOverlay {
        book_slug: request.book_slug.clone(),
        entry_key: request.entry_key.clone(),
        overlay: request.overlay.clone(),
        quality_flags: None,
        base_built_at: None,
        edit_reason: Some(request.reason.clone()),
        edited_at: edited_at.clone(),
    })?;
    Ok(ReferenceOverlaySetResult {
        book_slug: request.book_slug.clone(),
        entry_key: request.entry_key.clone(),
        edited_at,
    })
}

/// Stable error-class name for a failed overlay write's recorded row.
fn error_kind(err: &ReferenceError) -> &'static str {
    match err {
        ReferenceError::InvalidArgument(_) => "invalid_argument",
        ReferenceError::UnknownOverlayProperty { .. } => "unknown_overlay_property",
        ReferenceError::Store(_) => "reference_store",
        ReferenceError::Open { .. } => "reference_open",
        ReferenceError::Io { .. } => "reference_io",
        ReferenceError::Catalog(_) => "reference_catalog",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bookrack_catalog::Catalog;
    use bookrack_config::Config;
    use bookrack_embed::EmbedError;
    use bookrack_refs::{NewBook, NewEntry};
    use serde_json::json;

    use super::*;
    use crate::Caller;
    use crate::reads::reference::{ReferenceLookup, lookup_at};
    use crate::registry::{LibraryHandle, LibraryRegistry};
    use crate::write_session::{ChangeSink, Region, SessionRunError, WriteIntent};

    struct FakeEmbedder;

    impl Embedder for FakeEmbedder {
        async fn embed_batch(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<String>>);

    impl ChangeSink for RecordingSink {
        fn exclusive_write_started(&self, _intent: &WriteIntent) {}
        fn library_changed(&self, library: &str) {
            self.0.lock().expect("sink").push(library.to_string());
        }
        fn exclusive_write_ended(&self, _intent: &WriteIntent) {}
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        cfg: Arc<Config>,
        registry: Arc<LibraryRegistry<FakeEmbedder>>,
        handle: Arc<LibraryHandle<FakeEmbedder>>,
        sink: Arc<RecordingSink>,
    }

    /// One library, `alpha`, with a catalog and a reference store that
    /// holds one entry, `smith` of `book_a`.
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = Arc::new(Config::new(tmp.path().to_path_buf()));
        Catalog::open_for_write_unbacked(&cfg.catalog_db()).expect("seed catalog");
        let refs = Refs::open_for_write(&cfg.reference_db(), &cfg.backup_dir()).expect("seed refs");
        refs.upsert_book(&NewBook {
            book_slug: "book_a".to_string(),
            schema_name: "name_translation".to_string(),
            schema_version: 1,
            parser_version: "0.1.0".to_string(),
            title_zh: "book a".to_string(),
            title_en: None,
            edition: None,
            publisher: None,
            year: None,
            isbn: None,
            authority_rank: 10,
            built_at: "2026-06-25T00:00:00Z".to_string(),
            intake_id: None,
        })
        .expect("seed book");
        refs.upsert_entry(&NewEntry {
            book_slug: "book_a".to_string(),
            entry_key: "smith".to_string(),
            headword: "smith".to_string(),
            aliases: vec![],
            payload: json!({"country": "USA"}),
            fts_text: "smith".to_string(),
            source: json!({"book_slug": "book_a", "page": 1}),
            quality_flags: vec![],
        })
        .expect("seed entry");
        drop(refs);

        let ops = crate::Ops::catalog_only(
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            cfg.books_dir(),
            cfg.backup_dir(),
            Caller::cli(),
        );
        let handle = LibraryHandle::new("alpha", Arc::clone(&cfg), ops);
        let sink = Arc::new(RecordingSink::default());
        let registry = LibraryRegistry::from_handles_with_sink(
            [Arc::clone(&handle)],
            Some("alpha".into()),
            Arc::clone(&sink) as Arc<dyn ChangeSink>,
        )
        .expect("registry");
        Fixture {
            _tmp: tmp,
            cfg,
            registry,
            handle,
            sink,
        }
    }

    fn request(overlay: serde_json::Value) -> ReferenceOverlaySetRequest {
        ReferenceOverlaySetRequest {
            book_slug: "book_a".to_string(),
            entry_key: "smith".to_string(),
            overlay,
            reason: "fix the OCR confusion".to_string(),
        }
    }

    async fn run(
        fx: &Fixture,
        req: ReferenceOverlaySetRequest,
    ) -> Result<ReferenceOverlaySetResult, SessionRunError<ReferenceError>> {
        let session = fx
            .registry
            .begin_write(
                &fx.handle,
                Region::Reference {
                    book_slug: req.book_slug.clone(),
                },
                Caller::mcp(),
            )
            .expect("session");
        session
            .run_blocking(async move |s| set_overlay(s, &req))
            .await
    }

    fn recorded(fx: &Fixture) -> Vec<bookrack_catalog::McpToolCall> {
        Catalog::open(&fx.cfg.catalog_db())
            .expect("open catalog")
            .expect("catalog exists")
            .tool_calls_for_tool(OVERLAY_SET_TOOL)
            .expect("read tool calls")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_written_overlay_is_read_back_recorded_and_announced() {
        let fx = fixture();
        let receipt = run(&fx, request(json!({"country": "United States"})))
            .await
            .expect("the overlay is written");
        assert_eq!(
            (receipt.book_slug.as_str(), receipt.entry_key.as_str()),
            ("book_a", "smith")
        );

        let result = lookup_at(
            &fx.cfg.reference_db(),
            catalogs().expect("catalogs"),
            &ReferenceLookup {
                book: "book_a".to_string(),
                entry_key: "smith".to_string(),
                fields: None,
                min_severity: None,
                exclude_books: None,
            },
        )
        .expect("lookup");
        assert_eq!(result.hits[0].payload["country"], "United States");
        assert!(result.hits[0].has_overlay);

        let rows = recorded(&fx);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].status, "ok");
        assert_eq!(rows[0].source, "mcp", "attributed to the session's caller");
        assert_eq!(*fx.sink.0.lock().expect("sink"), ["alpha"]);
    }

    /// A payload key outside the property catalog is refused before the
    /// store is opened, recorded as such, and announces nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_property_is_refused_before_the_store_is_opened() {
        let fx = fixture();
        std::fs::remove_file(fx.cfg.reference_db()).expect("drop the store");
        let err = run(&fx, request(json!({"random_key": "value"})))
            .await
            .expect_err("the key is not in the catalog");
        assert!(
            matches!(
                &err,
                SessionRunError::Op(ReferenceError::UnknownOverlayProperty { key })
                    if key == "random_key"
            ),
            "{err:?}"
        );
        assert!(
            !fx.cfg.reference_db().exists(),
            "a refused payload must not create the store"
        );
        let rows = recorded(&fx);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0].error_type.as_deref(),
            Some("unknown_overlay_property")
        );
        assert!(fx.sink.0.lock().expect("sink").is_empty());
    }

    #[test]
    fn a_payload_that_is_not_an_object_is_refused() {
        let cats = catalogs().expect("catalogs");
        for overlay in [json!("United Kingdom"), json!(["country"]), json!(null)] {
            let err = validate_overlay(cats, &request(overlay.clone())).unwrap_err();
            assert!(
                matches!(err, ReferenceError::InvalidArgument(_)),
                "{overlay}: {err:?}"
            );
        }
    }

    /// A store this build cannot open keeps the path and the store's own
    /// cause: the front end words the schema states from them, and an
    /// unclassified failure is reported with its whole chain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_store_that_cannot_be_opened_keeps_its_path_and_cause() {
        let fx = fixture();
        std::fs::write(fx.cfg.reference_db(), b"this is not a sqlite database")
            .expect("write a foreign file");
        let err = run(&fx, request(json!({"country": "United Kingdom"})))
            .await
            .expect_err("a foreign file cannot be opened");
        let SessionRunError::Op(ReferenceError::Open { path, source }) = &err else {
            panic!("the open failure keeps its own variant: {err:?}");
        };
        assert_eq!(path, &fx.cfg.reference_db());
        let chain = bookrack_core::Problem::from_error_chain(&err).summary;
        assert!(
            chain.contains(&path.display().to_string()) && chain.contains(&source.to_string()),
            "the rendered chain names the store and its cause: {chain}"
        );
        assert_eq!(
            recorded(&fx)[0].error_type.as_deref(),
            Some("reference_open")
        );
    }
}
