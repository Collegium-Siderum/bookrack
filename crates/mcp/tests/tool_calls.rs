// SPDX-License-Identifier: Apache-2.0

//! Handler-level contract tests through the real MCP transport.
//!
//! Each test boots `bookrack_mcp::serve` over a registry whose single
//! handle wraps a real, freshly seeded catalog in a tempdir, connects
//! a real streamable-HTTP MCP client, and drives tool calls end to
//! end. The pinned contracts are the ones an agent client depends on
//! and no other layer asserts:
//!
//! * a successful tool response is one text content block whose text
//!   is the JSON body;
//! * `library.show_book` reads an unknown intake as a `null` body
//!   rather than an error;
//! * an unknown `library` selector, a caller-input edit fault, and a
//!   malformed `kind` all surface as `invalid_params`, while an
//!   environmental fault surfaces as `internal_error`;
//! * every recorded tool-call row carries `source = "mcp"` even
//!   though the shared `Ops` was built for another surface — the
//!   `call_tool` caller override the audit trail depends on.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_catalog::{Catalog, IntakeStatus, NewIntake};
use bookrack_config::Config;
use bookrack_core::ItemKind;
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorCode, JsonObject};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;

struct Fixture {
    _tmp: tempfile::TempDir,
    catalog_db: std::path::PathBuf,
    intake_id: i64,
    addr: SocketAddr,
    shutdown_tx: broadcast::Sender<()>,
    server: tokio::task::JoinHandle<eyre::Result<()>>,
}

impl Fixture {
    /// Seed a one-book catalog, boot `serve` on an ephemeral port, and
    /// wait until the listener accepts.
    async fn start() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let catalog_db = root.join("catalog.db");
        let intake_id = {
            let mut catalog = Catalog::open(&catalog_db).expect("open catalog");
            catalog
                .register_intake(
                    ItemKind::Book,
                    &NewIntake::new("sha-fixture").format("epub"),
                )
                .expect("register intake")
                .into_intake()
                .intake_id
        };

        // Deliberately a CLI-surface Ops: the `source = "mcp"`
        // assertion can then only pass through the `call_tool`
        // caller override.
        // The configuration names the same root the catalog above was
        // opened under, so the handle's two halves are one library's.
        let cfg = Arc::new(Config::new(root.clone(), "http://127.0.0.1:1".to_string()));
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            cfg.books_dir(),
            cfg.backup_dir(),
            Caller::cli(),
        );
        let registry = LibraryRegistry::single(LibraryHandle::new("fixture", cfg, ops));

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(4);
        let info_context = LibraryInfoContext {
            data_dir: "unused".to_string(),
            library_name: Some("fixture".to_string()),
            resolution_source: "test fixture".to_string(),
            shadowed_default: None,
            library_identification: None,
            ollama_url: "http://127.0.0.1:1".to_string(),
            embed_model_configured: "unused".to_string(),
            mcp_addr: addr.to_string(),
        };
        let serve_tx = shutdown_tx.clone();
        let server = tokio::spawn(async move {
            bookrack_mcp::serve(
                registry,
                info_context,
                Instant::now(),
                LogStreamHandle::default(),
                Arc::new(Mutex::new(QueueState::default())),
                serve_tx,
                listener,
                shutdown_rx,
            )
            .await
        });

        timeout(Duration::from_secs(5), async {
            loop {
                match TcpStream::connect(addr).await {
                    Ok(stream) => break drop(stream),
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("serve accepts a connection within 5s");

        Fixture {
            _tmp: tmp,
            catalog_db,
            intake_id,
            addr,
            shutdown_tx,
            server,
        }
    }

    /// Register one book and drive it to `status`, returning its
    /// intake id.
    fn seed_book(&self, sha: &str, status: IntakeStatus) -> i64 {
        let mut catalog = Catalog::open(&self.catalog_db).expect("open catalog");
        let intake_id = catalog
            .register_intake(ItemKind::Book, &NewIntake::new(sha).format("epub"))
            .expect("register intake")
            .into_intake()
            .intake_id;
        catalog
            .set_intake_status(ItemKind::Book, intake_id, status)
            .expect("set status");
        intake_id
    }

    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let transport =
            StreamableHttpClientTransport::from_uri(format!("http://{}/mcp", self.addr));
        ().serve(transport).await.expect("mcp client handshake")
    }

    async fn stop(self) {
        let _ = self.shutdown_tx.send(());
        let _ = timeout(Duration::from_secs(5), self.server).await;
    }
}

/// A tool call: `name` with a JSON-object argument payload.
fn call(name: &'static str, arguments: serde_json::Value) -> CallToolRequestParams {
    let arguments: JsonObject = arguments
        .as_object()
        .cloned()
        .expect("tool arguments are a JSON object");
    CallToolRequestParams::new(name).with_arguments(arguments)
}

/// Decode the single text content block a successful tool call
/// returns.
fn body_json(result: &CallToolResult) -> serde_json::Value {
    assert_ne!(
        result.is_error,
        Some(true),
        "tool reported an error: {result:?}"
    );
    assert_eq!(
        result.content.len(),
        1,
        "one text block per response: {result:?}"
    );
    let text = result.content[0]
        .as_text()
        .expect("a text content block")
        .text
        .clone();
    serde_json::from_str(&text).expect("tool body is JSON")
}

/// Unwrap the JSON-RPC error a rejected tool call surfaces as.
fn rpc_error(err: ServiceError) -> rmcp::model::ErrorData {
    match err {
        ServiceError::McpError(data) => data,
        other => panic!("expected an MCP error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_tool_call_round_trips_and_is_attributed_to_the_mcp_surface() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let result = client
        .call_tool(call("library.stats", serde_json::json!({})))
        .await
        .expect("library.stats");
    let body = body_json(&result);
    let by_status = body["intake_counts_by_status"]
        .as_object()
        .expect("counts map");
    let total: u64 = by_status.values().filter_map(|v| v.as_u64()).sum();
    assert_eq!(total, 1, "one seeded intake, got: {body}");
    assert_eq!(body["intake_count_by_format"]["epub"], 1);

    // The registry's Ops carries `Caller::cli()`, so only the
    // `call_tool` override can make the recorded source read "mcp".
    let catalog = Catalog::open(&fx.catalog_db).expect("reopen catalog");
    let rows = catalog
        .tool_calls_for_tool("library.stats")
        .expect("tool calls");
    assert_eq!(rows.len(), 1, "one recorded call");
    assert_eq!(rows[0].source, "mcp");
    assert_eq!(rows[0].status, "ok");

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn show_book_reads_an_unknown_intake_as_a_null_body() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let result = client
        .call_tool(call(
            "library.show_book",
            serde_json::json!({ "intake_id": 999_999 }),
        ))
        .await
        .expect("library.show_book");
    assert!(
        body_json(&result).is_null(),
        "an unknown intake must read as null, not an error"
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn an_unknown_library_selector_is_invalid_params() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let err = client
        .call_tool(call(
            "library.stats",
            serde_json::json!({ "library": "no-such-library" }),
        ))
        .await
        .expect_err("an unknown library selector must be rejected");
    let data = rpc_error(err);
    assert_eq!(data.code, ErrorCode::INVALID_PARAMS, "{}", data.message);
    assert!(
        data.message.contains("no-such-library"),
        "the message must name the rejected selector: {}",
        data.message
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn metadata_set_with_an_unknown_field_is_invalid_params() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let err = client
        .call_tool(call(
            "library.metadata.set",
            serde_json::json!({
                "library": "fixture",
                "intake_id": fx.intake_id,
                "field": "not_a_field",
                "value": "x",
                "reason": "contract test",
            }),
        ))
        .await
        .expect_err("an unknown metadata field must be rejected");
    let data = rpc_error(err);
    assert_eq!(data.code, ErrorCode::INVALID_PARAMS, "{}", data.message);
    assert!(
        data.message.contains("not_a_field"),
        "the message must name the rejected field: {}",
        data.message
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn search_distinguishes_caller_input_from_environmental_faults() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    // A kind outside the documented set is the caller's mistake.
    let err = client
        .call_tool(call(
            "library.search",
            serde_json::json!({ "query": "q", "kind": "papers" }),
        ))
        .await
        .expect_err("an unknown kind must be rejected");
    assert_eq!(rpc_error(err).code, ErrorCode::INVALID_PARAMS);

    // A catalog-only handle has no warm search backend: the same tool
    // with valid arguments fails as an internal (environmental) error.
    let err = client
        .call_tool(call("library.search", serde_json::json!({ "query": "q" })))
        .await
        .expect_err("search without a backend must fail");
    assert_eq!(rpc_error(err).code, ErrorCode::INTERNAL_ERROR);

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn find_books_filters_on_the_requested_lifecycle_statuses() {
    // Only an embedded book can be recalled by search. Without this
    // filter a client cannot tell a book with nothing to find from one
    // that has not reached the vector store yet.
    let fx = Fixture::start().await;
    let embedded = fx.seed_book("sha-embedded", IntakeStatus::Embedded);
    let _pending = fx.seed_book("sha-pending", IntakeStatus::Pending);
    let client = fx.connect().await;

    let result = client
        .call_tool(call(
            "library.find_books",
            serde_json::json!({ "statuses": ["embedded"] }),
        ))
        .await
        .expect("find_books with a status filter");
    let body = body_json(&result);
    let ids: Vec<i64> = body["books"]
        .as_array()
        .expect("books array")
        .iter()
        .map(|b| b["intake_id"].as_i64().expect("intake_id"))
        .collect();
    assert_eq!(
        ids,
        vec![embedded],
        "the status filter did not reach the catalog: {body}"
    );
    assert_eq!(
        body["total"].as_u64(),
        Some(1),
        "`total` counts the unfiltered shelf: {body}"
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn find_books_refuses_a_status_no_book_can_be_in() {
    // Dropping an unrecognised status would answer with the whole
    // shelf, which reads as a successful filter that matched
    // everything.
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let err = client
        .call_tool(call(
            "library.find_books",
            serde_json::json!({ "statuses": ["embeded"] }),
        ))
        .await
        .expect_err("a misspelled status must be rejected");
    let data = rpc_error(err);
    assert_eq!(data.code, ErrorCode::INVALID_PARAMS, "{}", data.message);
    assert!(
        data.message.contains("embeded"),
        "the message must name the rejected status: {}",
        data.message
    );
    // The accepted set is the remedy, so it belongs in the hint rather
    // than in the summary line.
    let problem: bookrack_core::ProblemData =
        serde_json::from_value(data.data.expect("data slot filled")).expect("ProblemData");
    let hint = problem.hint.expect("a rejected status has a next step");
    for status in IntakeStatus::ALL {
        assert!(
            hint.contains(status.as_str()),
            "the hint must state the accepted set, missing {}: {hint}",
            status.as_str(),
        );
    }

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn list_metadata_refuses_a_confidence_grade_no_audit_writes() {
    // The two vocabularies this listing filters on are closed sets. A
    // value outside one is refused with what would have been accepted,
    // rather than dropped into an unfiltered listing.
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let err = client
        .call_tool(call(
            "library.list_metadata",
            serde_json::json!({ "confidence_in": ["quite-sure"] }),
        ))
        .await
        .expect_err("a confidence grade outside the audit's set must be rejected");
    let data = rpc_error(err);
    assert_eq!(data.code, ErrorCode::INVALID_PARAMS, "{}", data.message);
    assert!(
        data.message.contains("quite-sure") && data.message.contains("confidence_in"),
        "the refusal must name the parameter and the value: {}",
        data.message
    );
    let problem: bookrack_core::ProblemData =
        serde_json::from_value(data.data.expect("data slot filled")).expect("ProblemData");
    let hint = problem.hint.expect("a refused value has a next step");
    for level in bookrack_catalog::CONFIDENCE_LEVELS {
        assert!(
            hint.contains(level),
            "the hint must state the accepted set, missing {level}: {hint}"
        );
    }

    // The same shape holds for the review-status vocabulary.
    let err = client
        .call_tool(call(
            "library.list_metadata",
            serde_json::json!({ "review_status_in": ["maybe"] }),
        ))
        .await
        .expect_err("a review status outside the set must be rejected");
    let data = rpc_error(err);
    assert_eq!(data.code, ErrorCode::INVALID_PARAMS, "{}", data.message);
    assert!(
        data.message.contains("review_status_in"),
        "the refusal must name the parameter: {}",
        data.message
    );

    let _ = client.cancel().await;
    fx.stop().await;
}
