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
//! * an unknown `library` selector, a caller-input edit fault, a
//!   write against an unknown intake, and a malformed `kind` all
//!   surface as `invalid_params`, while an environmental fault
//!   surfaces as `internal_error`;
//! * every recorded tool-call row carries `source = "mcp"` even
//!   though the shared `Ops` was built for another surface — the
//!   `call_tool` caller override the audit trail depends on;
//! * `library.info` for a selected library reports that library's
//!   root and name, not the ones of the library the daemon came up
//!   on;
//! * `session.info` lists each mounted library with its own root and
//!   default mark, and its default name follows the registry's pointer;
//! * `session.queue_status` filters `recent` on a state before capping
//!   it, counts the whole queue, refuses a state no job can be in, and
//!   carries a failure's cause without build-machine source prefixes;
//! * a client on protocol version 2026-07-28, served one request at a
//!   time, receives a tool's `invalid_params` in band under HTTP 200,
//!   and a request that neither initializes nor names a session is
//!   answered HTTP 400.
//!
//! The registry carries two libraries on two roots so a card whose
//! identity and counts came from different libraries is observable.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_catalog::{Catalog, IntakeStatus, NewIntake};
use bookrack_config::{Config, LibrarySelection};
use bookrack_core::ItemKind;
use bookrack_core::queue::{JobState, Priority, QueueJob, QueueState};
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use bookrack_test_support::{ProcessEnv, process_env};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorCode, JsonObject};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;

struct Fixture {
    _tmp: tempfile::TempDir,
    /// Root of the default library, `fixture`.
    root: PathBuf,
    /// Root of the second registered library, `second`.
    second_root: PathBuf,
    catalog_db: PathBuf,
    intake_id: i64,
    /// The registry `serve` routes through, held so a test can move the
    /// default pointer while the server runs.
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    /// The queue snapshot `serve` reads, held so a test can seed jobs.
    queue_state: Arc<Mutex<QueueState>>,
    addr: SocketAddr,
    shutdown_tx: broadcast::Sender<()>,
    server: tokio::task::JoinHandle<eyre::Result<()>>,
}

impl Fixture {
    /// Seed a one-book catalog under the default library's root, add an
    /// empty second library on its own root, boot `serve` on an
    /// ephemeral port, and wait until the listener accepts.
    async fn start() -> Fixture {
        // Both libraries resolve through the sandbox registry, as the
        // daemon's do, so each configuration carries its own name and
        // root; the sandbox also keeps the status card's registry read
        // off the machine running the tests.
        let sandbox = process_env(ProcessEnv::isolated());
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("fixture-root");
        let second_root = tmp.path().join("second-root");
        for dir in [&root, &second_root] {
            std::fs::create_dir_all(dir).expect("library root");
        }
        let (cfg, second_cfg) = {
            // The registry is one file per process: write and resolve
            // under a lock so concurrent fixtures see their own entries.
            static REGISTRY: Mutex<()> = Mutex::new(());
            let _guard = REGISTRY.lock().expect("registry lock");
            sandbox.write_registry_entries(
                Some("fixture"),
                &[
                    ("fixture", root.as_path()),
                    ("second", second_root.as_path()),
                ],
            );
            (resolve("fixture"), resolve("second"))
        };
        let catalog_db = root.join("catalog.db");
        // An empty catalog, so the second library is a real root rather
        // than a bare directory.
        Catalog::open(&second_root.join("catalog.db")).expect("open second catalog");
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

        let registry = LibraryRegistry::from_handles(
            [handle("fixture", cfg), handle("second", second_cfg)],
            "fixture",
        )
        .expect("two-library registry");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(4);
        // The bring-up snapshot names the default library, as the
        // daemon's does.
        let info_context = LibraryInfoContext {
            data_dir: root.display().to_string(),
            library_name: Some("fixture".to_string()),
            resolution_source: "test fixture".to_string(),
            shadowed_default: None,
            library_identification: None,
            ollama_url: "http://127.0.0.1:1".to_string(),
            embed_model_configured: "unused".to_string(),
            mcp_addr: addr.to_string(),
        };
        let serve_tx = shutdown_tx.clone();
        let serve_registry = registry.clone();
        let queue_state = Arc::new(Mutex::new(QueueState::default()));
        let serve_queue = queue_state.clone();
        let server = tokio::spawn(async move {
            bookrack_mcp::serve(
                serve_registry,
                info_context,
                Instant::now(),
                LogStreamHandle::default(),
                serve_queue,
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
            root,
            second_root,
            catalog_db,
            intake_id,
            registry,
            queue_state,
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

    /// Replace the queue snapshot with `jobs`, in enqueue order.
    fn seed_queue(&self, jobs: impl IntoIterator<Item = QueueJob>) {
        let mut queue = self.queue_state.lock().expect("queue state lock");
        queue.jobs = jobs.into_iter().collect();
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

/// Resolve the registry library `name` the way the daemon does for
/// each library it serves.
fn resolve(name: &str) -> Arc<Config> {
    Arc::new(
        Config::resolve(&LibrarySelection {
            data_dir: None,
            library: Some(name.to_string()),
        })
        .expect("resolve a registered library"),
    )
}

/// A library handle over `cfg`. Deliberately a CLI-surface Ops: the
/// `source = "mcp"` assertion can then only pass through the
/// `call_tool` caller override. The configuration names the same root
/// the catalog was opened under, so the handle's two halves are one
/// library's.
fn handle(name: &str, cfg: Arc<Config>) -> Arc<LibraryHandle<OllamaEmbedClient>> {
    let ops = Ops::<OllamaEmbedClient>::catalog_only(
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        cfg.books_dir(),
        cfg.backup_dir(),
        Caller::cli(),
    );
    LibraryHandle::new(name, cfg, ops)
}

/// A finished book job `job-<n>` in `state`, enqueued `n` seconds after
/// a fixed origin so that seeding jobs in order of `n` is enqueue order.
fn finished_job(n: u32, state: JobState, error: Option<&str>) -> QueueJob {
    finished_job_with(n, state, error, None)
}

/// [`finished_job`] with the classified diagnostic the daemon records
/// beside a failure it recognised.
fn finished_job_with(
    n: u32,
    state: JobState,
    error: Option<&str>,
    problem: Option<bookrack_core::Problem>,
) -> QueueJob {
    let at = chrono::DateTime::from_timestamp(1_700_000_000 + i64::from(n), 0).expect("timestamp");
    QueueJob {
        id: format!("job-{n:02}"),
        library: "fixture".to_string(),
        path: PathBuf::from(format!("/srv/books/{n:02}.epub")),
        priority: Priority::Normal,
        force: false,
        hold_for_metadata: false,
        kind: ItemKind::Book,
        intake_ocr: None,
        audit_profile: None,
        state,
        queued_at: at,
        started_at: Some(at),
        finished_at: Some(at),
        error: error.map(str::to_string),
        problem,
        merged_into: None,
    }
}

/// The ids of a `session.queue_status` body's `recent` rows, in order.
fn recent_ids(body: &serde_json::Value) -> Vec<String> {
    body["recent"]
        .as_array()
        .expect("recent list")
        .iter()
        .map(|row| row["id"].as_str().expect("row id").to_string())
        .collect()
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

/// Send one raw HTTP/1.1 `POST /mcp` carrying `headers` and `body`,
/// and return the status code with the whole response as text. The
/// MCP client folds a non-2xx status into a transport error, so the
/// status line is only observable on a raw socket.
async fn post_raw(addr: SocketAddr, headers: &[(&str, &str)], body: String) -> (u16, String) {
    use std::fmt::Write as _;
    use std::io::{Read, Write};

    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    request.push_str("\r\n");
    request.push_str(&body);

    tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect to /mcp");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        stream.write_all(request.as_bytes()).expect("write request");
        let mut raw = Vec::new();
        // A timeout ends the read with what has arrived; the asserts
        // below say whether that was enough.
        let _ = stream.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status = text
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("an HTTP status line, got: {text:?}"));
        (status, text)
    })
    .await
    .expect("raw request task")
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

/// A client that negotiates protocol version 2026-07-28 sends no
/// `initialize` and is served one request at a time. A tool's
/// `invalid_params` for the caller's input reaches it as a JSON-RPC
/// error under HTTP 200: a client that treats a 4xx status as a
/// transport failure never reads the body.
#[tokio::test]
async fn a_per_request_clients_invalid_params_arrives_in_band() {
    let fx = Fixture::start().await;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "library.stats",
            "arguments": { "library": "no-such-library" },
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let (status, text) = post_raw(
        fx.addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "library.stats"),
        ],
        body.to_string(),
    )
    .await;
    assert_eq!(status, 200, "invalid params must stay in band: {text}");
    assert!(
        text.contains("-32602") && text.contains("no-such-library"),
        "the body must carry the invalid-params error naming the selector: {text}"
    );

    fx.stop().await;
}

/// A request that is not `initialize` and names no session is
/// answered HTTP 400, the status the streamable-HTTP transport asks of
/// a server that requires a session.
#[tokio::test]
async fn a_session_less_request_that_is_not_initialize_is_bad_request() {
    let fx = Fixture::start().await;

    let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let (status, text) = post_raw(fx.addr, &[], body.to_string()).await;
    assert_eq!(
        status, 400,
        "a session-less request must be refused: {text}"
    );
    assert!(
        text.contains("Session ID is required"),
        "the refusal must name the missing session: {text}"
    );

    fx.stop().await;
}

/// `library.info { library }` reports the identity of the library it
/// was asked about. The counts on the card come from the selected
/// handle, so a card whose root and name came from the bring-up
/// snapshot would describe two libraries at once.
#[tokio::test]
async fn library_info_reports_the_selected_librarys_identity() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let result = client
        .call_tool(call(
            "library.info",
            serde_json::json!({ "library": "second" }),
        ))
        .await
        .expect("library.info for the second library");
    let body = body_json(&result);
    assert_eq!(
        body["library_name"].as_str(),
        Some("second"),
        "the card names another library: {body}"
    );
    assert_eq!(
        body["data_dir"].as_str(),
        Some(fx.second_root.display().to_string().as_str()),
        "the card reports another library's root: {body}"
    );

    // Without a selector the card describes the default library.
    let result = client
        .call_tool(call("library.info", serde_json::json!({})))
        .await
        .expect("library.info for the default library");
    let body = body_json(&result);
    assert_eq!(body["library_name"].as_str(), Some("fixture"), "{body}");
    assert_eq!(
        body["data_dir"].as_str(),
        Some(fx.root.display().to_string().as_str()),
        "{body}"
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

/// `session.info` lists every mounted library as a row carrying its own
/// root and default mark, and names the default from that same set. A
/// card with one top-level root beside a default name that follows the
/// registry's pointer pairs two libraries once the pointer moves off
/// the library the daemon came up on.
#[tokio::test]
async fn session_info_rows_carry_each_librarys_root_and_follow_the_default() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let before = body_json(
        &client
            .call_tool(call("session.info", serde_json::json!({})))
            .await
            .expect("session.info before the default moves"),
    );
    fx.registry
        .set_default("second")
        .expect("move the default pointer");
    let after = body_json(
        &client
            .call_tool(call("session.info", serde_json::json!({})))
            .await
            .expect("session.info after the default moves"),
    );
    let both = format!("before: {before}\nafter: {after}");

    assert!(
        after.get("data_dir").is_none(),
        "the card carries a root outside the library rows:\n{both}"
    );
    let root = fx.root.display().to_string();
    let second_root = fx.second_root.display().to_string();
    assert_eq!(
        before["libraries"],
        serde_json::json!([
            { "name": "fixture", "data_dir": root, "default": true },
            { "name": "second", "data_dir": second_root, "default": false },
        ]),
        "rows before the default moves:\n{both}"
    );
    assert_eq!(
        before["default_library"].as_str(),
        Some("fixture"),
        "{both}"
    );
    assert_eq!(
        after["libraries"],
        serde_json::json!([
            { "name": "fixture", "data_dir": root, "default": false },
            { "name": "second", "data_dir": second_root, "default": true },
        ]),
        "rows after the default moves:\n{both}"
    );
    assert_eq!(after["default_library"].as_str(), Some("second"), "{both}");

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
async fn a_write_tool_against_an_unknown_intake_is_invalid_params() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    // No tool arm special-cases this id, so the refusal reaches the
    // shared classifier and must come back as the caller's input.
    let err = client
        .call_tool(call(
            "library.metadata.approve",
            serde_json::json!({
                "library": "fixture",
                "intake_id": 999_999,
                "reason": "contract test",
            }),
        ))
        .await
        .expect_err("approving an unknown intake must be rejected");
    let data = rpc_error(err);
    assert_eq!(data.code, ErrorCode::INVALID_PARAMS, "{}", data.message);
    assert!(
        data.message.contains("999999"),
        "the message must name the rejected id: {}",
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

/// Twelve finished jobs, the earliest of them failed. Without a filter
/// the failure has scrolled out of the ten-row `recent` window; `state`
/// brings it back with its cause, and the counts still cover the whole
/// queue rather than the filtered rows.
#[tokio::test]
async fn queue_status_filters_on_state_before_capping_recent() {
    let fx = Fixture::start().await;
    fx.seed_queue(
        std::iter::once(finished_job_with(
            0,
            JobState::Failed,
            Some("ingest: extract error: no body blocks"),
            Some(
                bookrack_core::Problem::new("cannot ingest: the extraction produced no body")
                    .hint("Check that the source is not image-only."),
            ),
        ))
        .chain((1..=11).map(|n| finished_job(n, JobState::Done, None))),
    );
    let client = fx.connect().await;

    let body = body_json(
        &client
            .call_tool(call(
                "session.queue_status",
                serde_json::json!({ "state": "failed" }),
            ))
            .await
            .expect("session.queue_status for failed jobs"),
    );
    assert_eq!(
        recent_ids(&body),
        ["job-00"],
        "`state: failed` must list the one failed job, however early it was enqueued: {body}"
    );
    let row = &body["recent"][0];
    assert_eq!(row["state"], "failed", "{body}");
    assert_eq!(
        row["error"], "ingest: extract error: no body blocks",
        "a failed row must say why it failed: {body}"
    );
    assert_eq!(
        row["finished_at"], "2023-11-14T22:13:20Z",
        "a finished row must say when it finished: {body}"
    );
    assert_eq!(
        row["problem"]["hint"], "Check that the source is not image-only.",
        "a classified failure carries its diagnostic: {body}"
    );
    assert_eq!(row["problem"]["retryable"], false, "{body}");
    assert_eq!(
        (body["failed"].as_u64(), body["done"].as_u64()),
        (Some(1), Some(11)),
        "the counts cover the whole queue, not the filtered rows: {body}"
    );

    // Unfiltered, `recent` is the ten newest jobs, newest first, and a
    // job that did not fail carries no error.
    let body = body_json(
        &client
            .call_tool(call("session.queue_status", serde_json::json!({})))
            .await
            .expect("unfiltered session.queue_status"),
    );
    let newest: Vec<String> = (2..=11).rev().map(|n| format!("job-{n:02}")).collect();
    assert_eq!(recent_ids(&body), newest, "{body}");
    assert!(
        body["recent"][0].get("error").is_none(),
        "a done row carries no error field: {body}"
    );
    assert!(
        body["recent"][0].get("problem").is_none(),
        "a done row carries no problem field: {body}"
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

/// A dependency source location under a build-machine prefix, as a
/// recorded diagnostic may carry it.
const LOCATED: &str =
    "/disk/cache/cargo/registry/src/index.crates.io-0123456789abcdef/dep-1.0.0/src/train.rs:10:4";
/// What [`LOCATED`] reads as once redacted.
const KEPT: &str = "dep-1.0.0/src/train.rs:10:4";

/// The tool reads the queue state in memory, past the redaction a load
/// applies, so each string of a recorded diagnostic is redacted as the
/// row is projected.
#[tokio::test]
async fn queue_status_redacts_every_string_of_a_problem() {
    let fx = Fixture::start().await;
    fx.seed_queue(std::iter::once(finished_job_with(
        0,
        JobState::Failed,
        Some("ingest: train failed"),
        Some(
            bookrack_core::Problem::new(format!("failed at {LOCATED}"))
                .detail(format!("Detail at {LOCATED}."))
                .hint(format!("Hint at {LOCATED}.")),
        ),
    )));
    let client = fx.connect().await;

    let body = body_json(
        &client
            .call_tool(call("session.queue_status", serde_json::json!({})))
            .await
            .expect("session.queue_status"),
    );
    let problem = &body["recent"][0]["problem"];
    assert_eq!(problem["summary"], format!("failed at {KEPT}"), "{body}");
    assert_eq!(problem["detail"], format!("Detail at {KEPT}."), "{body}");
    assert_eq!(problem["hint"], format!("Hint at {KEPT}."), "{body}");

    let _ = client.cancel().await;
    fx.stop().await;
}

/// More failures than the cap: the filtered list is still capped, and
/// holds the newest failures, newest first.
#[tokio::test]
async fn queue_status_caps_a_filtered_list_at_the_newest_matches() {
    let fx = Fixture::start().await;
    // Even jobs failed, odd jobs done: twelve of each, interleaved, so
    // capping before filtering keeps only five of the failures.
    fx.seed_queue((0..24).map(|n| {
        if n % 2 == 0 {
            finished_job(n, JobState::Failed, Some("ingest: extract error"))
        } else {
            finished_job(n, JobState::Done, None)
        }
    }));
    let client = fx.connect().await;

    let body = body_json(
        &client
            .call_tool(call(
                "session.queue_status",
                serde_json::json!({ "state": "failed" }),
            ))
            .await
            .expect("session.queue_status for failed jobs"),
    );
    let newest_failures: Vec<String> = (4..24)
        .rev()
        .filter(|n| n % 2 == 0)
        .map(|n| format!("job-{n:02}"))
        .collect();
    assert_eq!(
        recent_ids(&body),
        newest_failures,
        "the filtered list must be the ten newest failures: {body}"
    );
    assert_eq!(body["failed"].as_u64(), Some(12), "{body}");

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn queue_status_refuses_a_state_no_job_can_be_in() {
    // Dropping an unrecognised state would answer with the unfiltered
    // window, which reads as "no job in that state" whenever the
    // window happens to hold none.
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let err = client
        .call_tool(call(
            "session.queue_status",
            serde_json::json!({ "state": "faild" }),
        ))
        .await
        .expect_err("a misspelled state must be rejected");
    let data = rpc_error(err);
    assert_eq!(data.code, ErrorCode::INVALID_PARAMS, "{}", data.message);
    assert!(
        data.message.contains("faild") && data.message.contains("state"),
        "the refusal must name the parameter and the value: {}",
        data.message
    );
    let problem: bookrack_core::ProblemData =
        serde_json::from_value(data.data.expect("data slot filled")).expect("ProblemData");
    let hint = problem.hint.expect("a refused state has a next step");
    for state in JobState::ALL {
        assert!(
            hint.contains(state.as_wire_str()),
            "the hint must state the accepted set, missing {}: {hint}",
            state.as_wire_str()
        );
    }

    // Every state a row reports is accepted, including on an empty
    // queue.
    for state in JobState::ALL {
        let body = body_json(
            &client
                .call_tool(call(
                    "session.queue_status",
                    serde_json::json!({ "state": state.as_wire_str() }),
                ))
                .await
                .unwrap_or_else(|e| panic!("state {} refused: {e:?}", state.as_wire_str())),
        );
        assert_eq!(recent_ids(&body), Vec::<String>::new(), "{body}");
    }

    let _ = client.cancel().await;
    fx.stop().await;
}

/// A failure's text leaves through `recent` with a dependency source
/// location crate-relative, as the error envelope carries it, even when
/// the text reached the queue snapshot unredacted.
#[tokio::test]
async fn queue_status_reports_a_failure_without_build_prefixes() {
    let fx = Fixture::start().await;
    fx.seed_queue([
        finished_job(0, JobState::Done, None),
        finished_job(
            1,
            JobState::Failed,
            Some(
                "ingest: train failed, /disk/cache/cargo/registry/src/\
                 index.crates.io-0123456789abcdef/dep-1.0.0/src/train.rs:10:4: train failed",
            ),
        ),
    ]);
    let client = fx.connect().await;

    let body = body_json(
        &client
            .call_tool(call("session.queue_status", serde_json::json!({})))
            .await
            .expect("session.queue_status"),
    );
    assert_eq!(
        body["recent"][0]["error"],
        "ingest: train failed, dep-1.0.0/src/train.rs:10:4: train failed",
        "the newest job failed, and its cause must arrive without the build prefix: {body}"
    );

    let _ = client.cancel().await;
    fx.stop().await;
}
