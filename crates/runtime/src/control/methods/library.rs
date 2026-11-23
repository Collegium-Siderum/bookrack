// SPDX-License-Identifier: Apache-2.0

//! The library methods of [`bookrack_ops::methods::LIBRARY_METHODS`],
//! served over the control socket.
//!
//! The method bodies live in the ops dispatcher, which both transports
//! call; this module supplies what the control plane contributes — the
//! caller every write records, the process-side half of `library.info`,
//! and the mapping of a judged failure onto a JSON-RPC envelope.

use bookrack_config::Config;
use bookrack_ops::Caller;
use bookrack_ops::methods::dispatch::{DispatchContext, HostContext, dispatch as run};
use bookrack_ops::methods::{MethodSpec, Surface};
use bookrack_ops::reads::info::LibraryInfoContext;
use serde_json::Value;

use super::MethodContext;
use crate::control::error_map::dispatch_err;
use crate::control::jsonrpc::RpcError;

/// The control plane's half of the methods that need the host.
struct ControlHost<'a> {
    ollama_url: &'a str,
    mcp_addr: &'a str,
}

impl HostContext for ControlHost<'_> {
    /// The card's identity is the library the handle names: the counts
    /// already come from that handle, and a card pairing one library's
    /// counts with another's root is the one an operator cannot read.
    /// The Ollama endpoint and the MCP address belong to the process.
    fn library_info_context(&self, cfg: &Config) -> eyre::Result<LibraryInfoContext> {
        crate::daemon::library_info_context_for(cfg, self.ollama_url, self.mcp_addr)
    }
}

/// Run `spec` for a control-plane call, attributed to
/// [`Caller::control_plane`].
pub(super) async fn dispatch(
    spec: &MethodSpec,
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let host = ControlHost {
        ollama_url: &ctx.daemon_config.ollama_url,
        mcp_addr: &ctx.info_context.mcp_addr,
    };
    let caller = Caller::control_plane();
    let dispatch_ctx = DispatchContext {
        registry: &ctx.registry,
        caller: &caller,
        host: &host,
    };
    run(&dispatch_ctx, spec, Surface::ControlPlane, params.clone())
        .await
        .map(|reply| reply.value)
        .map_err(dispatch_err)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bookrack_refs::{NewBook, NewEntry, Refs};
    use serde_json::{Value, json};

    use super::super::{DispatchOutcome, dispatch, test_method_context};
    use crate::control::jsonrpc::{
        INTERNAL_ERROR, INVALID_PARAMS, Request, RpcError, STATE_UNUSABLE,
    };

    /// Call `method` through the control plane's dispatcher on a
    /// one-library context rooted at `dir`.
    async fn call(dir: &Path, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let ctx = test_method_context(dir, Some("alpha"));
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: method.to_string(),
            params,
        };
        match dispatch(&req, &ctx).await? {
            DispatchOutcome::Result(value) => Ok(value),
            DispatchOutcome::Shutdown(value) => panic!("{method} is not a shutdown: {value}"),
        }
    }

    async fn lookup(dir: &Path, params: Value) -> Result<Value, RpcError> {
        call(dir, "reference.lookup", Some(params)).await
    }

    fn book(slug: &str, rank: i64) -> NewBook {
        NewBook {
            book_slug: slug.to_string(),
            schema_name: "name_translation".to_string(),
            schema_version: 1,
            parser_version: "0.1.0".to_string(),
            title_zh: format!("book {slug}"),
            title_en: None,
            edition: None,
            publisher: None,
            year: None,
            isbn: None,
            authority_rank: rank,
            built_at: "2026-06-25T00:00:00Z".to_string(),
            intake_id: None,
        }
    }

    fn entry(slug: &str, key: &str, headword: &str, payload: Value) -> NewEntry {
        NewEntry {
            book_slug: slug.to_string(),
            entry_key: key.to_string(),
            headword: headword.to_string(),
            aliases: vec![],
            payload,
            fts_text: headword.to_string(),
            source: json!({ "book_slug": slug, "page": 1, "sheet": 1 }),
            quality_flags: vec![],
        }
    }

    /// Two books, one entry each, under the same key.
    fn seed(dir: &Path) {
        let refs = Refs::open_for_write(&dir.join("reference.db"), &dir.join("backup"))
            .expect("create reference.db");
        refs.upsert_book(&book("book_a", 10)).unwrap();
        refs.upsert_book(&book("book_b", 5)).unwrap();
        refs.upsert_entry(&entry(
            "book_a",
            "ashgrovewren",
            "Ashgrove, Wren",
            json!({ "country": "UK" }),
        ))
        .unwrap();
        refs.upsert_entry(&entry(
            "book_b",
            "ashgrovewren",
            "Ashgrove, Wren",
            json!({ "country": "GB" }),
        ))
        .unwrap();
    }

    fn stamp_user_version(dir: &Path, version: i64) {
        let conn =
            bookrack_dbkit::open_production(&dir.join("reference.db")).expect("open reference.db");
        conn.pragma_update(None, "user_version", version)
            .expect("stamp the version");
    }

    /// A read whose params are all optional takes none at all, absent
    /// or `null`.
    #[tokio::test]
    async fn a_read_with_only_optional_params_takes_none() {
        let dir = tempfile::tempdir().unwrap();
        for params in [None, Some(Value::Null)] {
            let page = call(dir.path(), "library.list_books", params.clone())
                .await
                .unwrap_or_else(|e| panic!("{params:?}: {e:?}"));
            assert_eq!(page["books"], json!([]), "{page}");
        }
    }

    /// A call missing a required field is refused as invalid params
    /// naming the method.
    #[tokio::test]
    async fn a_missing_required_field_is_refused_naming_the_method() {
        let dir = tempfile::tempdir().unwrap();
        let err = call(dir.path(), "library.show_book", None)
            .await
            .expect_err("intake_id is required");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("library.show_book"), "{}", err.message);
    }

    #[tokio::test]
    async fn an_exact_key_answers_its_hits_in_authority_order() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let body = lookup(
            dir.path(),
            json!({ "book": "*", "entry_key": "ashgrovewren" }),
        )
        .await
        .unwrap();
        assert_eq!(body["entry_key"], "ashgrovewren");
        assert_eq!(body["hits"].as_array().unwrap().len(), 2);
        assert_eq!(body["hits"][0]["book_slug"], "book_a");
        assert_eq!(body["primary_by_authority"], 0);
        assert_eq!(body["redirect_followed"], Value::Null);
    }

    #[tokio::test]
    async fn the_headword_as_written_reaches_the_entry_through_its_latin_form() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let body = lookup(
            dir.path(),
            json!({ "book": "book_a", "entry_key": "Ashgrove, Wren" }),
        )
        .await
        .unwrap();
        assert_eq!(body["entry_key"], "Ashgrove, Wren", "the query is echoed");
        assert_eq!(body["hits"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["hits"][0]["entry_key"], "ashgrovewren",
            "the hit carries the canonical key"
        );
        assert_eq!(body["redirect_followed"], Value::Null);
    }

    #[tokio::test]
    async fn a_key_nobody_wrote_answers_no_hits() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let body = lookup(
            dir.path(),
            json!({ "book": "book_a", "entry_key": "nobody" }),
        )
        .await
        .unwrap();
        assert_eq!(body["entry_key"], "nobody");
        assert_eq!(body["hits"], json!([]));
        assert_eq!(body["primary_by_authority"], Value::Null);
    }

    #[tokio::test]
    async fn a_library_without_a_reference_store_answers_no_hits() {
        let dir = tempfile::tempdir().unwrap();
        let body = lookup(
            dir.path(),
            json!({ "book": "book_a", "entry_key": "ashgrovewren" }),
        )
        .await
        .unwrap();
        assert_eq!(body["entry_key"], "ashgrovewren");
        assert_eq!(body["hits"], json!([]));
        assert!(
            !dir.path().join("reference.db").exists(),
            "looking must not create the store"
        );
    }

    #[tokio::test]
    async fn a_bad_argument_is_refused_whether_or_not_the_store_exists() {
        let bad = [
            (json!({ "book": "", "entry_key": "x" }), "`book`"),
            (
                json!({ "book": "book_a", "entry_key": "x", "exclude_books": ["book_b"] }),
                "`exclude_books`",
            ),
            (
                json!({ "book": "*", "entry_key": "x", "min_severity": "severe" }),
                "min_severity",
            ),
        ];
        for seeded in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            if seeded {
                seed(dir.path());
            }
            for (params, field) in &bad {
                let err = lookup(dir.path(), params.clone())
                    .await
                    .expect_err("a bad argument is refused");
                assert_eq!(err.code, INVALID_PARAMS, "seeded={seeded}: {err:?}");
                assert!(
                    err.message.contains(field),
                    "seeded={seeded}: the message names the field: {}",
                    err.message
                );
            }
        }
    }

    #[tokio::test]
    async fn a_store_path_that_is_a_directory_is_an_error_not_an_empty_answer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("reference.db")).unwrap();
        let err = lookup(dir.path(), json!({ "book": "book_a", "entry_key": "x" }))
            .await
            .expect_err("a directory where the store should be is a fault");
        assert_eq!(err.code, INTERNAL_ERROR, "{err:?}");
    }

    #[tokio::test]
    async fn a_store_from_a_newer_build_is_unusable_with_its_path_in_the_detail() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        stamp_user_version(dir.path(), bookrack_refs::TARGET_VERSION + 1);
        let err = lookup(dir.path(), json!({ "book": "book_a", "entry_key": "x" }))
            .await
            .expect_err("a newer schema is refused");
        assert_eq!(err.code, STATE_UNUSABLE, "{err:?}");
        assert_eq!(err.message, "cannot read the reference store");
        let data = err.data.expect("detail and hint");
        let detail = data["detail"].as_str().expect("detail");
        let store = dir.path().join("reference.db").display().to_string();
        assert!(detail.contains(&store), "detail names the store: {detail}");
        assert!(
            detail.contains(&format!("schema {}", bookrack_refs::TARGET_VERSION + 1)),
            "{detail}"
        );
        let hint = data["hint"].as_str().expect("hint");
        assert!(hint.contains("older than the store"), "{hint}");
    }

    #[tokio::test]
    async fn a_store_from_an_older_build_is_unusable_with_the_rebuild_in_the_hint() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        stamp_user_version(dir.path(), 0);
        let err = lookup(dir.path(), json!({ "book": "book_a", "entry_key": "x" }))
            .await
            .expect_err("an older schema is refused");
        assert_eq!(err.code, STATE_UNUSABLE, "{err:?}");
        let data = err.data.expect("detail and hint");
        let hint = data["hint"].as_str().expect("hint");
        assert!(hint.contains("distill build"), "{hint}");
    }
}
