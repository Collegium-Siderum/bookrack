// SPDX-License-Identifier: Apache-2.0

//! `reference.lookup`: one keyed read over a library's reference
//! store, mirroring the MCP tool of the same name. Both surfaces run
//! [`bookrack_ops::reads::reference::lookup_at`], so they accept the
//! same parameters, answer the same body and take the same error
//! codes.

use bookrack_ops::reads::reference::{self, ReferenceLookup};
use serde::Deserialize;
use serde_json::Value;

use super::MethodContext;
use super::reads_library::{parse, resolve, to_value};
use crate::control::error_map::reference_lookup_err;
use crate::control::jsonrpc::RpcError;

/// The `reference.lookup` params: the library selector beside the
/// lookup request itself.
#[derive(Debug, Deserialize)]
pub struct ReferenceLookupParams {
    #[serde(default)]
    pub library: Option<String>,
    #[serde(flatten)]
    pub lookup: ReferenceLookup,
}

routed_params!(ReferenceLookupParams);

pub fn lookup(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let p: ReferenceLookupParams = parse(params, "reference.lookup")?;
    let handle = resolve(ctx, p.library.as_deref())?;
    let catalogs = reference::catalogs().map_err(reference_lookup_err)?;
    let result = reference::lookup_at(&handle.ops().reference_db_path(), catalogs, &p.lookup)
        .map_err(reference_lookup_err)?;
    to_value(&result)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bookrack_refs::{NewBook, NewEntry, Refs};
    use serde_json::json;

    use super::super::test_method_context;
    use super::*;
    use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, STATE_UNUSABLE};

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
        let refs = Refs::open(&dir.join("reference.db")).expect("create reference.db");
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

    fn call(dir: &Path, params: Value) -> Result<Value, RpcError> {
        let ctx = test_method_context(dir, Some("alpha"));
        lookup(&Some(params), &ctx)
    }

    #[test]
    fn an_exact_key_answers_its_hits_in_authority_order() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let body = call(
            dir.path(),
            json!({ "book": "*", "entry_key": "ashgrovewren" }),
        )
        .unwrap();
        assert_eq!(body["entry_key"], "ashgrovewren");
        assert_eq!(body["hits"].as_array().unwrap().len(), 2);
        assert_eq!(body["hits"][0]["book_slug"], "book_a");
        assert_eq!(body["primary_by_authority"], 0);
        assert_eq!(body["redirect_followed"], Value::Null);
    }

    #[test]
    fn the_headword_as_written_reaches_the_entry_through_its_latin_form() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let body = call(
            dir.path(),
            json!({ "book": "book_a", "entry_key": "Ashgrove, Wren" }),
        )
        .unwrap();
        assert_eq!(body["entry_key"], "Ashgrove, Wren", "the query is echoed");
        assert_eq!(body["hits"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["hits"][0]["entry_key"], "ashgrovewren",
            "the hit carries the canonical key"
        );
        assert_eq!(body["redirect_followed"], Value::Null);
    }

    #[test]
    fn a_key_nobody_wrote_answers_no_hits() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let body = call(
            dir.path(),
            json!({ "book": "book_a", "entry_key": "nobody" }),
        )
        .unwrap();
        assert_eq!(body["entry_key"], "nobody");
        assert_eq!(body["hits"], json!([]));
        assert_eq!(body["primary_by_authority"], Value::Null);
    }

    #[test]
    fn a_library_without_a_reference_store_answers_no_hits() {
        let dir = tempfile::tempdir().unwrap();
        let body = call(
            dir.path(),
            json!({ "book": "book_a", "entry_key": "ashgrovewren" }),
        )
        .unwrap();
        assert_eq!(body["entry_key"], "ashgrovewren");
        assert_eq!(body["hits"], json!([]));
        assert!(
            !dir.path().join("reference.db").exists(),
            "looking must not create the store"
        );
    }

    #[test]
    fn a_bad_argument_is_refused_whether_or_not_the_store_exists() {
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
                let err = call(dir.path(), params.clone()).expect_err("a bad argument is refused");
                assert_eq!(err.code, INVALID_PARAMS, "seeded={seeded}: {err:?}");
                assert!(
                    err.message.contains(field),
                    "seeded={seeded}: the message names the field: {}",
                    err.message
                );
            }
        }
    }

    #[test]
    fn a_store_path_that_is_a_directory_is_an_error_not_an_empty_answer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("reference.db")).unwrap();
        let err = call(dir.path(), json!({ "book": "book_a", "entry_key": "x" }))
            .expect_err("a directory where the store should be is a fault");
        assert_eq!(err.code, INTERNAL_ERROR, "{err:?}");
    }

    #[test]
    fn a_store_from_a_newer_build_is_unusable_with_its_path_in_the_detail() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        stamp_user_version(dir.path(), bookrack_refs::TARGET_VERSION + 1);
        let err = call(dir.path(), json!({ "book": "book_a", "entry_key": "x" }))
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

    #[test]
    fn a_store_from_an_older_build_is_unusable_with_the_rebuild_in_the_hint() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        stamp_user_version(dir.path(), 0);
        let err = call(dir.path(), json!({ "book": "book_a", "entry_key": "x" }))
            .expect_err("an older schema is refused");
        assert_eq!(err.code, STATE_UNUSABLE, "{err:?}");
        let data = err.data.expect("detail and hint");
        let hint = data["hint"].as_str().expect("hint");
        assert!(hint.contains("distill build"), "{hint}");
    }
}
