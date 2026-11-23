// SPDX-License-Identifier: Apache-2.0

//! bookrack MCP server: exposes the daemon's method tables to agent
//! clients over streamable HTTP.
//!
//! The server is a thin shell. Its tools are registered at run time
//! from two tables: the rows of
//! [`bookrack_ops::methods::LIBRARY_METHODS`] MCP carries, and the
//! session tools of
//! [`bookrack_runtime::control::methods::session::SESSION_TOOLS`]. A
//! tool's name, description, and schemas are its row's, and its body is
//! that table's dispatcher, the one the control plane calls too; this
//! crate adds the MCP envelope around a reply and moves a judged
//! failure onto the MCP error envelope. The `library` selector routes a
//! call through the registry; absent, the registry routes the call as
//! it does one naming no library — the default when it is mounted,
//! else the only mounted library, else a refusal.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use axum::response::sse::{Event, KeepAlive, Sse};
use bookrack_core::DaemonIdentity;
use bookrack_core::queue::QueueState;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::Caller;
use bookrack_ops::methods::dispatch::{DispatchContext, Reply, dispatch};
use bookrack_ops::methods::{LIBRARY_METHODS, MethodSpec, Output, Surface};
use bookrack_runtime::control::events::EventStreamHandle;
use bookrack_runtime::control::methods::session::{
    self, SESSION_TOOLS, SessionContext, SessionTool,
};
use bookrack_runtime::control::methods::{DaemonRegistry, ProcessHost};
use eyre::WrapErr;
use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, ServerHandler, tool_handler};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

mod error_map;
use error_map::{dispatch_error_to_mcp, session_error_to_mcp};

/// The two endpoints of the daemon process the tools report: where
/// this listener answers and where the embedder is reached. Neither
/// belongs to a library; everything a tool reports about a library
/// comes from that library's handle.
#[derive(Debug, Clone)]
pub struct ProcessEndpoints {
    /// MCP listener address in `host:port` form, or `disabled` when the
    /// daemon runs without `/mcp`.
    pub mcp_addr: String,
    /// Ollama HTTP endpoint the daemon reaches.
    pub ollama_url: String,
}

/// Every tool's route, built once per process. A session clones it:
/// the clones share each tool's schemas, and each keeps its own
/// handler state.
static TOOL_ROUTER: LazyLock<ToolRouter<BookrackServer>> = LazyLock::new(tool_router);

/// MCP request handler. The streamable-HTTP service clones it per
/// session; the heavy state sits behind an `Arc`, so a clone is cheap
/// and every session shares one warm library registry.
#[derive(Clone)]
pub struct BookrackServer {
    registry: Arc<DaemonRegistry>,
    endpoints: ProcessEndpoints,
    started_at: Instant,
    identity: DaemonIdentity,
    log_stream: LogStreamHandle,
    queue_state: Arc<Mutex<QueueState>>,
    shutdown_tx: broadcast::Sender<()>,
    event_stream: EventStreamHandle,
    tool_router: ToolRouter<BookrackServer>,
}

impl BookrackServer {
    /// Build a handler over the given library registry. `endpoints`
    /// are the process's listener address and Ollama endpoint, which
    /// `session.info` and `library.info` report; every library's name,
    /// root, and status come from the registry and the selected
    /// handle. `started_at` stamps the daemon's birth so
    /// `session.info` can report uptime; `identity` is the daemon's
    /// own, reported by the same tool. `log_stream` is the shared
    /// in-process log fan-out the `session.logs_tail` tool reads its
    /// ring-buffer snapshot from. `queue_state` is the shared snapshot
    /// of the ingest queue the daemon's worker drives; a host that
    /// spawns no queue worker passes an inert default. `shutdown_tx`
    /// carries the session-wide graceful-shutdown signal so the
    /// `session.shutdown` tool can ask the daemon to stop, and
    /// `event_stream` is where that tool publishes the `stopping` state
    /// first.
    // One handle per subsystem the tools reach, each passed once by the
    // single caller; see `serve` for why they stay separate arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<DaemonRegistry>,
        endpoints: ProcessEndpoints,
        started_at: Instant,
        identity: DaemonIdentity,
        log_stream: LogStreamHandle,
        queue_state: Arc<Mutex<QueueState>>,
        shutdown_tx: broadcast::Sender<()>,
        event_stream: EventStreamHandle,
    ) -> BookrackServer {
        BookrackServer {
            registry,
            endpoints,
            started_at,
            identity,
            log_stream,
            queue_state,
            shutdown_tx,
            event_stream,
            tool_router: TOOL_ROUTER.clone(),
        }
    }

    /// Run the library method `spec` for a tool call carrying
    /// `arguments`, attributed to [`Caller::mcp`].
    async fn call_library(
        &self,
        spec: &'static MethodSpec,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ErrorData> {
        let host = ProcessHost {
            ollama_url: &self.endpoints.ollama_url,
            mcp_addr: &self.endpoints.mcp_addr,
        };
        let caller = Caller::mcp();
        let ctx = DispatchContext {
            registry: &*self.registry,
            caller: &caller,
            host: &host,
        };
        let reply = dispatch(&ctx, spec, Surface::Mcp, arguments.map(Value::Object))
            .await
            .map_err(dispatch_error_to_mcp)?;
        let structured = match spec.output {
            Output::Structured(_) => true,
            Output::NullableObject => reply.value.is_object(),
            Output::TextOnly => false,
        };
        Ok(tool_result(reply, structured))
    }

    /// Run the session tool `tool` for a call carrying `arguments`.
    fn call_session(
        &self,
        tool: &'static SessionTool,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ErrorData> {
        let ctx = SessionContext {
            registry: &self.registry,
            identity: &self.identity,
            started_at: self.started_at,
            mcp_addr: &self.endpoints.mcp_addr,
            ollama_url: &self.endpoints.ollama_url,
            log_stream: &self.log_stream,
            queue_state: &self.queue_state,
            event_stream: &self.event_stream,
            shutdown_tx: &self.shutdown_tx,
        };
        let reply = session::dispatch(&ctx, tool, arguments.map(Value::Object))
            .map_err(session_error_to_mcp)?;
        Ok(tool_result(reply, true))
    }
}

/// A successful tool answer: the reply's text as the one content block,
/// fields in their declared order, and the same answer as structured
/// content when `structured`.
fn tool_result(reply: Reply, structured: bool) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(reply.text)]);
    if structured {
        result.structured_content = Some(reply.value);
    }
    result
}

/// The route of every tool MCP carries: one per row of the library
/// table with an MCP name, then one per session tool.
///
/// A tool whose answer is always an object publishes its output schema;
/// one whose answer may be `null` or an array publishes none.
fn tool_router() -> ToolRouter<BookrackServer> {
    let mut router = ToolRouter::new();
    for spec in LIBRARY_METHODS {
        let Some(name) = spec.mcp_name else { continue };
        let mut tool = Tool::new(name, spec.summary, Arc::new((spec.mcp_input_schema)()));
        if let Output::Structured(schema) = spec.output {
            tool = tool.with_raw_output_schema(Arc::new(schema()));
        }
        router.add_route(ToolRoute::new_dyn(tool, move |tcc| library_call(spec, tcc)));
    }
    for session_tool in SESSION_TOOLS {
        let tool = Tool::new(
            session_tool.name,
            session_tool.summary,
            Arc::new((session_tool.input_schema)()),
        )
        .with_raw_output_schema(Arc::new((session_tool.output_schema)()));
        router.add_route(ToolRoute::new_dyn(tool, move |tcc| {
            session_call(session_tool, tcc)
        }));
    }
    router
}

/// What a tool's route returns: the call's answer, once the call has
/// run.
type ToolFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<CallToolResponse, ErrorData>> + Send + 'a>,
>;

/// The future a tool call of library method `spec` runs.
fn library_call<'a>(
    spec: &'static MethodSpec,
    tcc: ToolCallContext<'a, BookrackServer>,
) -> ToolFuture<'a> {
    let server = tcc.service;
    let arguments = tcc.arguments;
    Box::pin(async move { server.call_library(spec, arguments).await.map(Into::into) })
}

/// The future a tool call of session tool `tool` runs.
fn session_call<'a>(
    tool: &'static SessionTool,
    tcc: ToolCallContext<'a, BookrackServer>,
) -> ToolFuture<'a> {
    let server = tcc.service;
    let arguments = tcc.arguments;
    Box::pin(async move { server.call_session(tool, arguments).map(Into::into) })
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BookrackServer {
    /// The published identity is this workspace's own, not the SDK's:
    /// rmcp's default fills `serverInfo` from the environment of the
    /// crate that compiled it, which names the SDK rather than the
    /// server. Agent clients list what this returns, and
    /// `bookrack doctor` matches the name to tell this daemon apart
    /// from another service on the same address.
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                bookrack_config::MCP_SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Search and browse a local, offline library of books. Tools: \
             `library.stats` (counts), `library.list_books` / `library.find_books` \
             (browse and search the registry), `library.show_book` / `library.show_toc` \
             (per-book metadata and table of contents), `library.search` (vector \
             search across the whole library), `library.search_in_book` (vector \
             search confined to one book). Curation tools also write: the \
             `library.metadata.*` family edits a book's bibliographic record and \
             review status, and `reference.overlay_set` edits a reference entry."
                    .to_string(),
            )
    }
}

/// Serve the streamable-HTTP surface on an already-bound `listener`
/// until the shutdown channel fires.
///
/// The socket arrives bound rather than as an address to bind: the
/// daemon takes it during bring-up
/// ([`bookrack_runtime::mcp_endpoint::bind_listener`]) so a port it
/// cannot have refuses the whole start, instead of failing inside a
/// task nobody observes until shutdown.
///
/// Two HTTP routes are mounted:
///
/// * `/mcp` — the MCP streamable-HTTP service. Connect an MCP client
///   to `http://<addr>/mcp`. Every tool resolves its target library
///   through the registry — by the `library` selector when present,
///   by the registry's rule for an unnamed call when absent. Write tools
///   require an explicit `library` name in their input; read tools
///   accept `library` as optional.
/// * `/session/logs` — a Server-Sent Events endpoint that streams
///   `LogEvent`s from `log_stream` as soon as they are produced. Each
///   SSE frame's `data:` payload is one log event serialised as JSON;
///   the stream stays open until the client disconnects or the
///   daemon shuts down.
///
/// `endpoints` are the listener address and Ollama endpoint the
/// `session.info` and `library.info` tools report.
///
/// `started_at` stamps the wall-clock instant the host daemon
/// considers itself "up"; `session.info` reports the elapsed seconds
/// against it. Callers typically capture this at the very top of their
/// `main` so the reported uptime spans configuration resolution and
/// embedding-store warm-up, not just the listener's lifetime.
///
/// `log_stream` is the in-process log fan-out handle returned by
/// `bookrack_obs::init`; the `session.logs_tail` tool reads its ring
/// buffer through it and the `/session/logs` SSE endpoint subscribes
/// to its broadcast channel.
///
/// `queue_state` is the shared snapshot of the ingest queue the
/// daemon's worker drives; a host that spawns no queue worker passes an
/// inert default.
///
/// `shutdown_tx` is the session-wide graceful-shutdown broadcaster
/// that the signal listener, the control-plane socket, the queue
/// worker, and this listener all subscribe to. The `session.shutdown` MCP tool fires it, after
/// publishing the `stopping` state on `event_stream`; this function also subscribes through `shutdown_rx` (the
/// receiver side of the same broadcast) for its own listener loop.
// Eight separate handles are the natural shape here: each is owned by a
// distinct subsystem (registry, endpoints, log fan-out, queue, shutdown
// fan-in/out, listener address) that the caller threaded through its
// own state machine. Bundling them into a struct buys nothing — the
// only two call sites are already passing each handle exactly once —
// and would just trade names for repetition.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    registry: Arc<DaemonRegistry>,
    endpoints: ProcessEndpoints,
    started_at: Instant,
    identity: DaemonIdentity,
    log_stream: LogStreamHandle,
    queue_state: Arc<Mutex<QueueState>>,
    shutdown_tx: broadcast::Sender<()>,
    event_stream: EventStreamHandle,
    listener: tokio::net::TcpListener,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    let log_stream_for_sse = log_stream.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(BookrackServer::new(
                registry.clone(),
                endpoints.clone(),
                started_at,
                identity.clone(),
                log_stream.clone(),
                queue_state.clone(),
                shutdown_tx.clone(),
                event_stream.clone(),
            ))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service).route(
        "/session/logs",
        axum::routing::get(move || {
            let handle = log_stream_for_sse.clone();
            sse_logs_handler(handle)
        }),
    );
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    tracing::info!(%addr, "bookrack MCP server listening on /mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .context("serve MCP server")?;
    Ok(())
}

/// SSE handler for `/session/logs`. Subscribes to the shared
/// [`LogStreamHandle`] broadcast channel and emits each event as one
/// SSE frame whose `data:` payload is the JSON-serialised event.
///
/// Receivers that fall behind get a `Lagged` error from the broadcast
/// channel and are silently skipped — the SSE stream stays open and
/// catches up with subsequent events rather than tearing the
/// connection down. Events whose JSON serialisation fails (which
/// should not happen for a well-formed `LogEvent`) are dropped the
/// same way.
///
/// `KeepAlive::default()` emits a comment-only frame every 15 seconds
/// so proxies that prune idle TCP connections leave the stream alone
/// during quiet periods.
async fn sse_logs_handler(
    handle: LogStreamHandle,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = handle.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(
        |item| -> Option<Result<Event, std::convert::Infallible>> {
            let ev = item.ok()?;
            let data = serde_json::to_string(&ev).ok()?;
            Some(Ok(Event::default().data(data)))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Spawn the MCP listener as a session-scoped task against a running
/// [`DaemonRuntime`](bookrack_runtime::DaemonRuntime), taking the
/// socket the runtime bound during bring-up. Returns `None` when the
/// runtime came up with MCP disabled (`--no-mcp`), so the daemon runs
/// without an HTTP surface. Shared by every daemon host (CLI, GUI,
/// headless) so the listener wiring has one source.
///
/// Takes `&mut` because the bound socket moves out of the runtime and
/// into the task: a second call finds nothing left to serve and is a
/// caller-side bug, reported as such rather than silently leaving the
/// session without the endpoint it announced.
pub fn spawn_listener(
    runtime: &mut bookrack_runtime::DaemonRuntime,
) -> Option<tokio::task::JoinHandle<eyre::Result<()>>> {
    let Some(listener) = runtime.mcp_listener.take() else {
        if runtime.mcp_label == "disabled" {
            tracing::info!("MCP listener disabled (--no-mcp); session running without /mcp");
        } else {
            tracing::error!(
                mcp = %runtime.mcp_label,
                "MCP socket already taken; session running without /mcp",
            );
        }
        return None;
    };
    let registry = Arc::clone(&runtime.registry);
    let endpoints = ProcessEndpoints {
        mcp_addr: runtime.mcp_label.clone(),
        ollama_url: runtime.daemon_config.ollama_url.clone(),
    };
    let started_at = runtime.started_at;
    let identity = runtime.identity.clone();
    let log_stream = runtime.log_stream.clone();
    let queue_state = Arc::clone(&runtime.queue_state);
    let shutdown_tx = runtime.shutdown_tx.clone();
    let event_stream = runtime.event_stream.clone();
    let rx = runtime.shutdown_tx.subscribe();
    Some(tokio::spawn(async move {
        serve(
            registry,
            endpoints,
            started_at,
            identity,
            log_stream,
            queue_state,
            shutdown_tx,
            event_stream,
            listener,
            rx,
        )
        .await
    }))
}

/// Enumerate every MCP tool the live server exposes, read from the
/// routes the server registers, so the list cannot name a tool the
/// server does not answer.
///
/// The daemon's control plane reaches for this list at startup so
/// `daemon.mcp_tools` can answer without spinning up an MCP transport.
pub fn list_tools() -> Vec<bookrack_runtime::control::methods::meta::McpToolInfo> {
    TOOL_ROUTER
        .list_all()
        .into_iter()
        .map(
            |tool| bookrack_runtime::control::methods::meta::McpToolInfo {
                name: tool.name.to_string(),
                description: tool.description.map(|d| d.to_string()).unwrap_or_default(),
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use bookrack_core::ItemKind;
    use bookrack_core::NodeId;
    use bookrack_ops::Citation;
    use bookrack_ops::dto::{
        BookDetail, BookSummary, ContextWindow, ContributorEntry, LibraryStats, ListBooksResult,
        Passage, SpanText, Toc, TocNode, TocNodes, TocSlimEntry,
    };

    fn citation(node: i64) -> Citation {
        Citation {
            text: "passage".to_string(),
            breadcrumb: "A Test Book \u{203a} Chapter One".to_string(),
            intake_id: NodeId::new(node).partition().get(),
            kind: ItemKind::Book,
            toc_position: Some(0),
            enclosing_node_id: Some(NodeId::new(node).partition().root()),
            start_node_id: NodeId::new(node),
            start_char_offset: 0,
            end_node_id: NodeId::new(node),
            end_char_offset: 7,
            norm_chunk_sha256: "sha".to_string(),
            distance: 0.1,
            rerank_score: None,
        }
    }

    #[test]
    fn search_citations_serialize_as_an_array_with_bare_int_node_ids() {
        let json = serde_json::to_string(&[citation(100_000_001)]).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(value.is_array());
        assert_eq!(value[0]["start_node_id"], serde_json::json!(100_000_001));
        assert_eq!(value[0]["breadcrumb"], "A Test Book \u{203a} Chapter One");
    }

    #[test]
    fn book_summary_serializes_with_owned_strings() {
        let summary = BookSummary {
            intake_id: 1,
            title: Some("A Title".to_string()),
            format: Some("epub".to_string()),
            status: "extracted".to_string(),
            top_contributor: Some("An Author".to_string()),
            source_filename: Some("book.epub".to_string()),
            awaiting_text: None,
        };
        let value = serde_json::to_value(&summary).expect("serialize");
        assert_eq!(value["intake_id"], 1);
        assert_eq!(value["title"], "A Title");
        assert_eq!(value["status"], "extracted");
        assert_eq!(value["source_filename"], "book.epub");
    }

    #[test]
    fn book_detail_carries_an_ordered_effective_biblio_map() {
        let mut biblio = std::collections::BTreeMap::new();
        biblio.insert("title".to_string(), "T".to_string());
        biblio.insert("publisher".to_string(), "P".to_string());
        let detail = BookDetail {
            intake_id: 7,
            title: Some("T".to_string()),
            format: Some("pdf".to_string()),
            status: "embedded".to_string(),
            source_path: Some("library/book.pdf".to_string()),
            source_filename: Some("book.pdf".to_string()),
            source_sha256: "0".repeat(64),
            intake_at: "2026-01-01T00:00:00Z".to_string(),
            page_count: Some(612),
            byte_size: Some(4096),
            effective_biblio: biblio,
            overrides: Vec::new(),
            contributors: vec![ContributorEntry {
                contributor_id: 1,
                role: "author".to_string(),
                ordinal: 0,
                name: "An Author".to_string(),
                nationality: None,
                origin: "extracted".to_string(),
            }],
            toc_stats: Some(bookrack_ops::dto::TocStats {
                entry_count: 885,
                max_depth: 2,
            }),
        };
        let value = serde_json::to_value(&detail).expect("serialize");
        assert_eq!(value["toc_stats"]["entry_count"], 885);
        assert_eq!(value["toc_stats"]["max_depth"], 2);
        assert_eq!(value["effective_biblio"]["title"], "T");
        assert_eq!(value["contributors"][0]["role"], "author");
        assert_eq!(value["source_filename"], "book.pdf");
        assert_eq!(value["source_path"], "library/book.pdf");
        assert_eq!(value["intake_at"], "2026-01-01T00:00:00Z");
        assert_eq!(value["page_count"], 612);
        assert_eq!(value["byte_size"], 4096);
        assert!(
            value["source_sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64)
        );
    }

    #[test]
    fn list_books_result_carries_total_and_truncated_flag() {
        let page = ListBooksResult {
            books: Vec::new(),
            total: 42,
            truncated: true,
        };
        let value = serde_json::to_value(&page).expect("serialize");
        assert_eq!(value["total"], 42);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn empty_library_stats_serialize_as_empty_maps() {
        let stats = LibraryStats::default();
        let value = serde_json::to_value(&stats).expect("serialize");
        assert!(value["intake_counts_by_status"].is_object());
        assert_eq!(
            value["intake_counts_by_status"].as_object().unwrap().len(),
            0
        );
    }

    #[test]
    fn toc_serializes_with_its_pagination_fields() {
        let toc = Toc {
            intake_id: 1,
            nodes: TocNodes::Full(vec![TocNode {
                node_id: 100_000_001,
                parent_id: None,
                title: Some("Root".to_string()),
                depth: 0,
                ordinal: 0,
                toc_lo: Some(1),
                toc_hi: Some(50),
            }]),
            total: 3,
            next_offset: Some(1),
            truncated: true,
        };
        let value = serde_json::to_value(&toc).expect("serialize");
        assert_eq!(value["intake_id"], 1);
        assert_eq!(value["nodes"][0]["node_id"], 100_000_001);
        assert_eq!(value["nodes"][0].as_object().unwrap().len(), 7);
        assert_eq!(value["total"], 3);
        assert_eq!(value["next_offset"], 1);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn slim_toc_entries_serialize_as_a_bare_three_field_array() {
        let toc = Toc {
            intake_id: 1,
            nodes: TocNodes::Slim(vec![TocSlimEntry {
                node_id: 100_000_001,
                title: Some("Chapter One".to_string()),
                depth: 1,
            }]),
            total: 1,
            next_offset: None,
            truncated: false,
        };
        let value = serde_json::to_value(&toc).expect("serialize");
        // The untagged enum must not wrap the array in a variant tag.
        assert!(value["nodes"].is_array());
        let entry = value["nodes"][0].as_object().expect("entry object");
        assert_eq!(entry.len(), 3);
        assert_eq!(entry["node_id"], 100_000_001);
        assert_eq!(entry["title"], "Chapter One");
        assert_eq!(entry["depth"], 1);
    }

    #[test]
    fn context_window_serializes_with_its_anchor_and_passages() {
        let window = ContextWindow {
            intake_id: 1,
            anchor_node_id: 100_000_002,
            passages: vec![Passage {
                node_id: 100_000_002,
                node_type: "paragraph".to_string(),
                toc_position: 7,
                page_index_start: Some(3),
                text: "the passage body".to_string(),
            }],
            truncated: false,
        };
        let value = serde_json::to_value(&window).expect("serialize");
        assert_eq!(value["intake_id"], 1);
        assert_eq!(value["anchor_node_id"], 100_000_002);
        assert_eq!(value["passages"][0]["node_type"], "paragraph");
        assert_eq!(value["passages"][0]["toc_position"], 7);
        assert_eq!(value["passages"][0]["text"], "the passage body");
        assert_eq!(value["truncated"], false);
    }

    #[test]
    fn span_text_serializes_with_its_cursor() {
        let span = SpanText {
            intake_id: 1,
            node_id: 100_000_001,
            title: Some("Chapter One".to_string()),
            toc_lo: Some(0),
            toc_hi: Some(42),
            passages: Vec::new(),
            next_offset: Some(17),
            truncated: true,
        };
        let value = serde_json::to_value(&span).expect("serialize");
        assert_eq!(value["node_id"], 100_000_001);
        assert_eq!(value["title"], "Chapter One");
        assert_eq!(value["toc_lo"], 0);
        assert_eq!(value["next_offset"], 17);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn paper_summary_serializes_paper_specific_fields() {
        use bookrack_ops::dto::PaperSummary;
        let summary = PaperSummary {
            intake_id: 1,
            title: Some("On Test Spaces".to_string()),
            format: Some("pdf".to_string()),
            status: "embedded".to_string(),
            top_contributor: Some("First Author".to_string()),
            doi: Some("10.5555/x.1".to_string()),
            arxiv_id: Some("0000.00001".to_string()),
            container_title: Some("Synthetic Journal".to_string()),
            year: Some("2020".to_string()),
            source_filename: Some("paper.pdf".to_string()),
        };
        let value = serde_json::to_value(&summary).expect("serialize");
        assert_eq!(value["intake_id"], 1);
        assert_eq!(value["doi"], "10.5555/x.1");
        assert_eq!(value["arxiv_id"], "0000.00001");
        assert_eq!(value["container_title"], "Synthetic Journal");
        assert_eq!(value["year"], "2020");
        assert_eq!(value["source_filename"], "paper.pdf");
    }

    #[test]
    fn library_stats_papers_section_is_omitted_when_absent() {
        let stats = LibraryStats {
            intake_counts_by_status: std::collections::BTreeMap::new(),
            intake_count_by_format: std::collections::BTreeMap::new(),
            book_state_counts_by_stage: std::collections::BTreeMap::new(),
            retrieval_issue_counts_by_status: std::collections::BTreeMap::new(),
            papers: None,
        };
        let value = serde_json::to_value(&stats).expect("serialize");
        assert!(
            value.get("papers").is_none(),
            "the papers section must be omitted when the book-only handle reports stats"
        );
    }

    #[test]
    fn the_find_books_schema_does_not_disclaim_the_category_filter() {
        // A field's doc comment becomes that parameter's `description`
        // in the published tool schema, which is all a client reads
        // before deciding whether a parameter is worth sending. A
        // disclaimer there makes an implemented, tested filter
        // unreachable in practice, however the tool description and
        // `docs/control-plane.md` word it.
        let tool = super::TOOL_ROUTER
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "library.find_books")
            .expect("library.find_books is a published tool");
        let schema = serde_json::to_string(&tool.input_schema).expect("serialise input schema");

        for disclaimer in ["not honoured", "Reserved hook", "future release"] {
            assert!(
                !schema.contains(disclaimer),
                "the parameter schema says {disclaimer:?} about a filter the server applies: \
                 {schema}"
            );
        }
        assert!(
            schema.contains("categories"),
            "the schema carries no `categories` parameter at all: {schema}"
        );
    }
}
