// SPDX-License-Identifier: Apache-2.0

//! `reference.lookup` answers alike on both surfaces.
//!
//! The MCP tool is driven through a real streamable-HTTP client; the
//! control-plane method of the same name through the runtime's
//! `dispatch`, over the same registry. The pinned contracts:
//!
//! * for one library and one set of parameters the two surfaces return
//!   byte-equal bodies — a direct hit, the latin-form fallback, a
//!   followed redirect, a `fields` projection and a `min_severity`
//!   filter — and, when they fail, the same error code;
//! * a library with no reference store answers no hits from the tool;
//! * a reference store at a schema newer than this build is
//!   `state unusable` from the `reference.overlay_set` tool as it is
//!   from the lookup tool, with the same summary, and the overlay is not
//!   written.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_config::{Config, LibrarySelection};
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use bookrack_refs::{NewBook, NewEntry, Refs};
use bookrack_runtime::control::jsonrpc::{INVALID_PARAMS, Request, STATE_UNUSABLE};
use bookrack_runtime::control::methods::{DispatchOutcome, MethodContext, dispatch};
use bookrack_runtime::control::plan_registry::PlanRegistry;
use bookrack_runtime::control::{DaemonState, DaemonStateFlag, EventStreamHandle};
use bookrack_test_support::{ProcessEnv, process_env};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorCode, JsonObject};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;

struct Fixture {
    _tmp: tempfile::TempDir,
    /// Root of `with`, the library that carries a reference store.
    with_root: PathBuf,
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    queue_state: Arc<Mutex<QueueState>>,
    addr: SocketAddr,
    shutdown_tx: broadcast::Sender<()>,
    server: tokio::task::JoinHandle<eyre::Result<()>>,
}

impl Fixture {
    /// Two libraries on two roots: `with` carries a seeded reference
    /// store, `without` has none. `serve` is booted over both.
    async fn start() -> Fixture {
        let sandbox = process_env(ProcessEnv::isolated());
        let tmp = tempfile::tempdir().expect("tempdir");
        let with_root = tmp.path().join("with-root");
        let without_root = tmp.path().join("without-root");
        for dir in [&with_root, &without_root] {
            std::fs::create_dir_all(dir).expect("library root");
        }
        let (with_cfg, without_cfg) = {
            static REGISTRY: Mutex<()> = Mutex::new(());
            let _guard = REGISTRY.lock().expect("registry lock");
            sandbox.write_registry_entries(
                Some("with"),
                &[
                    ("with", with_root.as_path()),
                    ("without", without_root.as_path()),
                ],
            );
            (resolve("with"), resolve("without"))
        };
        seed(&with_cfg.reference_db());

        let registry = LibraryRegistry::from_handles(
            [handle("with", with_cfg), handle("without", without_cfg)],
            Some("with".into()),
        )
        .expect("two-library registry");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(4);
        let queue_state = Arc::new(Mutex::new(QueueState::default()));
        let serve_tx = shutdown_tx.clone();
        let serve_registry = registry.clone();
        let serve_queue = queue_state.clone();
        let info_context = info_context(&with_root, addr);
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
            with_root,
            registry,
            queue_state,
            addr,
            shutdown_tx,
            server,
        }
    }

    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let transport =
            StreamableHttpClientTransport::from_uri(format!("http://{}/mcp", self.addr));
        ().serve(transport).await.expect("mcp client handshake")
    }

    /// A control-plane method context over the registry `serve` is
    /// answering on, the shape the daemon dispatches with.
    fn method_context(&self) -> MethodContext {
        let state = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        MethodContext {
            registry: self.registry.clone(),
            info_context: info_context(&self.with_root, self.addr),
            queue_state: self.queue_state.clone(),
            queue_state_path: self.with_root.join("queue.json"),
            event_stream: EventStreamHandle::new(8, state),
            write_guard: Arc::new(tokio::sync::Mutex::new(())),
            shutdown_tx: self.shutdown_tx.clone(),
            started_at_rfc3339: "2026-01-01T00:00:00Z".to_string(),
            selection: LibrarySelection::default(),
            library_name: "with".to_string(),
            auto_registered: Vec::new(),
            mcp_tools: Arc::new(Vec::new()),
            queue_worker_enabled: false,
            rerank_supervisor: None,
            control_socket: None,
            queue_paused: Arc::new(AtomicBool::new(false)),
            log_stream: LogStreamHandle::new(8, 8),
            plan_registry: Arc::new(PlanRegistry::new()),
            mounter: None,
        }
    }

    /// `reference.lookup` over the control plane, through the router.
    async fn control_plane(&self, params: Value) -> Result<Value, (i32, String)> {
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "reference.lookup".to_string(),
            params: Some(params),
        };
        match dispatch(&req, &self.method_context()).await {
            Ok(DispatchOutcome::Result(value)) => Ok(value),
            Ok(DispatchOutcome::Shutdown(_)) => panic!("a read method asked to shut down"),
            Err(e) => Err((e.code, e.message)),
        }
    }

    /// Stamp the `with` store with a `user_version` this build does not
    /// read.
    fn stamp_newer_schema(&self) {
        let conn = bookrack_dbkit::open_production(&self.with_root.join("reference.db"))
            .expect("open reference.db");
        conn.pragma_update(None, "user_version", bookrack_refs::TARGET_VERSION + 1)
            .expect("stamp the version");
    }

    async fn stop(self) {
        let _ = self.shutdown_tx.send(());
        let _ = timeout(Duration::from_secs(5), self.server).await;
    }
}

fn info_context(root: &Path, addr: SocketAddr) -> LibraryInfoContext {
    LibraryInfoContext {
        data_dir: root.display().to_string(),
        library_name: Some("with".to_string()),
        resolution_source: "test fixture".to_string(),
        shadowed_default: None,
        library_identification: None,
        ollama_url: "http://127.0.0.1:1".to_string(),
        embed_model_configured: "unused".to_string(),
        mcp_addr: addr.to_string(),
    }
}

fn resolve(name: &str) -> Arc<Config> {
    Arc::new(
        Config::resolve(&LibrarySelection {
            data_dir: None,
            library: Some(name.to_string()),
        })
        .expect("resolve a registered library"),
    )
}

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

fn entry(slug: &str, key: &str, payload: Value, flags: &[&str]) -> NewEntry {
    NewEntry {
        book_slug: slug.to_string(),
        entry_key: key.to_string(),
        headword: key.to_string(),
        aliases: vec![],
        payload,
        fts_text: key.to_string(),
        source: json!({ "book_slug": slug, "page": 1, "sheet": 1 }),
        quality_flags: flags.iter().map(|f| f.to_string()).collect(),
    }
}

/// Two books. `smith` is in both, with a payload wide enough to
/// project and a warn-severity flag on one side only; `jones` redirects
/// to `smith` in `book_a`; `ashgrovewren` is reachable from its
/// headword through the latin fallback.
fn seed(path: &Path) {
    let refs = Refs::open(path).expect("create reference.db");
    refs.upsert_book(&book("book_a", 10)).unwrap();
    refs.upsert_book(&book("book_b", 5)).unwrap();
    refs.upsert_entry(&entry(
        "book_a",
        "smith",
        json!({ "country": "UK", "bio_annotation": "a" }),
        &[],
    ))
    .unwrap();
    refs.upsert_entry(&entry(
        "book_b",
        "smith",
        json!({ "country": "GB", "bio_annotation": "b" }),
        &["pair_mismatch"],
    ))
    .unwrap();
    refs.upsert_entry(&entry(
        "book_a",
        "jones",
        json!({ "redirect_to": "smith" }),
        &[],
    ))
    .unwrap();
    refs.upsert_entry(&entry(
        "book_a",
        "ashgrovewren",
        json!({ "country": "UK" }),
        &[],
    ))
    .unwrap();
}

fn call(name: &'static str, arguments: Value) -> CallToolRequestParams {
    let arguments: JsonObject = arguments
        .as_object()
        .cloned()
        .expect("tool arguments are a JSON object");
    CallToolRequestParams::new(name).with_arguments(arguments)
}

fn body_json(result: &CallToolResult) -> Value {
    assert_ne!(
        result.is_error,
        Some(true),
        "tool reported an error: {result:?}"
    );
    assert_eq!(result.content.len(), 1, "one text block: {result:?}");
    let text = result.content[0]
        .as_text()
        .expect("a text content block")
        .text
        .clone();
    serde_json::from_str(&text).expect("tool body is JSON")
}

fn rpc_error(err: ServiceError) -> rmcp::model::ErrorData {
    match err {
        ServiceError::McpError(data) => data,
        other => panic!("expected an MCP error, got {other:?}"),
    }
}

#[tokio::test]
async fn the_control_plane_and_mcp_answer_a_lookup_alike() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let successes = [
        (
            "a direct hit",
            json!({ "library": "with", "book": "book_a", "entry_key": "smith" }),
        ),
        (
            "the latin fallback",
            json!({ "library": "with", "book": "book_a", "entry_key": "Ashgrove, Wren" }),
        ),
        (
            "a followed redirect",
            json!({ "library": "with", "book": "book_a", "entry_key": "jones" }),
        ),
        (
            "a fields projection",
            json!({ "library": "with", "book": "*", "entry_key": "smith", "fields": ["country"] }),
        ),
        (
            "a severity filter",
            json!({ "library": "with", "book": "*", "entry_key": "smith", "min_severity": "warn" }),
        ),
    ];
    for (case, params) in successes {
        let over_mcp = body_json(
            &client
                .call_tool(call("reference.lookup", params.clone()))
                .await
                .unwrap_or_else(|e| panic!("{case}: the tool answers: {e:?}")),
        );
        let over_rpc = fx
            .control_plane(params)
            .await
            .unwrap_or_else(|e| panic!("{case}: the method answers: {e:?}"));
        assert_eq!(over_mcp, over_rpc, "{case}: the two surfaces answer alike");
        assert!(
            !over_mcp["hits"].as_array().expect("hits").is_empty(),
            "{case}: the case is a hit"
        );
    }

    let bad = json!({ "library": "with", "book": "", "entry_key": "smith" });
    let over_mcp = rpc_error(
        client
            .call_tool(call("reference.lookup", bad.clone()))
            .await
            .expect_err("an empty book is refused"),
    );
    let (code, _) = fx
        .control_plane(bad)
        .await
        .expect_err("an empty book is refused");
    assert_eq!(over_mcp.code, ErrorCode(code));
    assert_eq!(code, INVALID_PARAMS);

    fx.stamp_newer_schema();
    let params = json!({ "library": "with", "book": "book_a", "entry_key": "smith" });
    let over_mcp = rpc_error(
        client
            .call_tool(call("reference.lookup", params.clone()))
            .await
            .expect_err("a newer schema is refused"),
    );
    let (code, _) = fx
        .control_plane(params)
        .await
        .expect_err("a newer schema is refused");
    assert_eq!(over_mcp.code, ErrorCode(code));
    assert_eq!(code, STATE_UNUSABLE);

    client.cancel().await.expect("client shutdown");
    fx.stop().await;
}

#[tokio::test]
async fn a_library_without_reference_books_answers_no_hits() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let body = body_json(
        &client
            .call_tool(call(
                "reference.lookup",
                json!({ "library": "without", "book": "book_a", "entry_key": "smith" }),
            ))
            .await
            .expect("a library without a store still answers"),
    );
    assert_eq!(body["entry_key"], "smith");
    assert_eq!(body["hits"], json!([]));
    assert_eq!(body["primary_by_authority"], Value::Null);

    client.cancel().await.expect("client shutdown");
    fx.stop().await;
}

#[tokio::test]
async fn a_newer_store_is_unusable_to_the_overlay_as_to_the_lookup() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;
    fx.stamp_newer_schema();

    let overlay = rpc_error(
        client
            .call_tool(call(
                "reference.overlay_set",
                json!({
                    "library": "with",
                    "book_slug": "book_a",
                    "entry_key": "smith",
                    "overlay": { "country": "United Kingdom" },
                    "reason": "test",
                }),
            ))
            .await
            .expect_err("the overlay cannot open a newer store"),
    );
    assert_eq!(overlay.code, ErrorCode(STATE_UNUSABLE), "{overlay:?}");
    assert_eq!(
        overlay.message, "cannot read the reference store",
        "{overlay:?}"
    );
    let data = overlay.data.clone().expect("detail and hint");
    assert!(
        data["detail"]
            .as_str()
            .is_some_and(|d| d.contains(&format!("schema {}", bookrack_refs::TARGET_VERSION + 1))),
        "{overlay:?}"
    );
    let version: i64 = bookrack_dbkit::open_production(&fx.with_root.join("reference.db"))
        .expect("open reference.db")
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("user_version");
    assert_eq!(
        version,
        bookrack_refs::TARGET_VERSION + 1,
        "the refused overlay must not have migrated the store"
    );

    let lookup = rpc_error(
        client
            .call_tool(call(
                "reference.lookup",
                json!({ "library": "with", "book": "book_a", "entry_key": "smith" }),
            ))
            .await
            .expect_err("the lookup cannot open a newer store"),
    );
    assert_eq!(lookup.code, ErrorCode(STATE_UNUSABLE), "{lookup:?}");

    client.cancel().await.expect("client shutdown");
    fx.stop().await;
}
