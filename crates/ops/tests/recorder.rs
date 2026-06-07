// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the tool-call recorder.
//!
//! Each test builds a tempdir-backed catalog-only [`Ops`], runs one
//! public op against it, then opens the catalog to inspect
//! `mcp_tool_calls`. The aim is to verify that the macros wrap the
//! body without altering the result, write a row with the right
//! `source` / `tool` / `status` / `args`, and survive failure paths
//! intact.

use std::path::PathBuf;

use bookrack_catalog::{Catalog, McpToolCall, NewIntake};
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::reads::books::{list_books, show_book};
use bookrack_ops::reads::info::{LibraryInfoContext, show_library_info};
use bookrack_ops::{Caller, Ops, OpsError};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
    catalog_db: PathBuf,
}

impl Fixture {
    fn cli() -> Fixture {
        Fixture::with_caller(Caller::cli())
    }

    fn mcp() -> Fixture {
        Fixture::with_caller(Caller::mcp())
    }

    fn with_caller(caller: Caller) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let catalog_db = tmp.path().join("catalog.db");
        let corpus_db = tmp.path().join("corpus.db");
        let lancedb_dir = tmp.path().join("lancedb");
        Catalog::open(&catalog_db).expect("seed catalog");
        let books_dir = tmp.path().join("books");
        let backup_dir = tmp.path().join("backup");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            corpus_db,
            catalog_db.clone(),
            &lancedb_dir,
            books_dir,
            backup_dir,
            caller,
        );
        Fixture {
            _tmp: tmp,
            ops,
            catalog_db,
        }
    }

    fn catalog(&self) -> Catalog {
        Catalog::open(&self.catalog_db).expect("open catalog")
    }

    fn seed_intake(&self, sha: &str) -> i64 {
        self.catalog()
            .register_intake(&NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id
    }

    fn rows(&self, tool: &str) -> Vec<McpToolCall> {
        self.catalog().tool_calls_for_tool(tool).expect("read")
    }
}

#[test]
fn record_call_sync_writes_one_ok_row() {
    let fx = Fixture::cli();
    list_books(&fx.ops, 10, 0).expect("list books");
    let rows = fx.rows("library.list_books");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "ok");
    assert_eq!(rows[0].source, "cli");
    assert!(rows[0].duration_ms.is_some());
}

#[test]
fn record_call_sync_writes_one_error_row_on_unknown_intake() {
    let fx = Fixture::cli();
    let err = show_book(&fx.ops, 9999).expect_err("must miss");
    assert!(matches!(err, OpsError::IntakeNotFound { intake_id: 9999 }));
    let rows = fx.rows("library.show_book");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "error");
    assert_eq!(rows[0].error_type.as_deref(), Some("intake_not_found"));
}

#[test]
fn record_call_records_args_as_json() {
    let fx = Fixture::cli();
    list_books(&fx.ops, 25, 7).expect("list books");
    let rows = fx.rows("library.list_books");
    let args = rows[0].args.as_deref().expect("args present");
    // Parse rather than string-compare so the column order does not
    // matter and a future field addition does not break this assert.
    let parsed: serde_json::Value = serde_json::from_str(args).expect("json parse");
    assert_eq!(parsed["limit"], 25);
    assert_eq!(parsed["offset"], 7);
}

#[test]
fn record_call_sync_distinguishes_cli_from_mcp_caller() {
    let cli = Fixture::cli();
    list_books(&cli.ops, 10, 0).expect("list books");
    assert_eq!(cli.rows("library.list_books")[0].source, "cli");

    let mcp = Fixture::mcp();
    list_books(&mcp.ops, 10, 0).expect("list books");
    assert_eq!(mcp.rows("library.list_books")[0].source, "mcp");
}

#[test]
fn record_call_sync_records_a_successful_write_op() {
    use bookrack_ops::dto::writes::SetMetadataFieldRequest;
    use bookrack_ops::writes::metadata::set_metadata_field;

    let fx = Fixture::cli();
    let id = fx.seed_intake("sha-recorder");
    set_metadata_field(
        &fx.ops,
        SetMetadataFieldRequest {
            intake_id: id,
            field: "title".to_string(),
            value: "Recorded".to_string(),
        },
    )
    .expect("set");

    let rows = fx.rows("library.metadata.set");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "ok");
    let parsed: serde_json::Value =
        serde_json::from_str(rows[0].args.as_deref().unwrap()).expect("json");
    assert_eq!(parsed["intake_id"], id);
    assert_eq!(parsed["field"], "title");
    assert_eq!(parsed["value"], "Recorded");
}

#[tokio::test]
async fn record_call_async_writes_one_ok_row() {
    let fx = Fixture::cli();
    let ctx = LibraryInfoContext {
        data_dir: fx.catalog_db.display().to_string(),
        library_name: None,
        resolution_source: "test".to_string(),
        ollama_url: "http://localhost:0/".to_string(),
        embed_model_configured: "test-model".to_string(),
    };
    show_library_info(&fx.ops, ctx).await.expect("info");
    let rows = fx.rows("library.info");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "ok");
    assert_eq!(rows[0].source, "cli");
    assert!(rows[0].args.is_none(), "info takes no args");
}
