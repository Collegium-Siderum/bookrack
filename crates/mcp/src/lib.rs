// SPDX-License-Identifier: Apache-2.0

//! bookrack MCP server: exposes the shared ops layer to agent clients
//! over streamable HTTP.
//!
//! The server is a thin shell. It holds the warm [`LibraryRegistry`]
//! behind an `Arc` and routes every tool call through it — the
//! tool's `library` selector picks the target handle, or falls back
//! to the registry's current default when absent. The crate depends
//! only on `bookrack-ops`, never on the database crates behind it, so
//! a schema change downstream leaves it untouched.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use axum::response::sse::{Event, KeepAlive, Sse};
use bookrack_core::queue::{JobState, QueueState};
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::{LogEvent, LogStreamHandle};
use bookrack_ops::dto::BookFilter;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, OpsError, SearchOptions, reads, with_caller_override, writes};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

/// Arguments for the `library.search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// The natural-language query to search the library for.
    pub query: String,
    /// Maximum number of passages to return. Defaults to the server's
    /// configured top-k when omitted.
    pub top_k: Option<usize>,
    /// Force a brute-force scan, ignoring any ANN index. Useful for
    /// ground-truth checks.
    #[serde(default)]
    pub bypass_index: bool,
    /// Override the IVF probe count for this query only. Higher values
    /// trade latency for recall; the persisted meta default applies when
    /// omitted.
    #[serde(default)]
    pub nprobes: Option<usize>,
    /// Override the IVF-PQ refinement multiplier for this query only.
    /// The persisted meta default applies when omitted.
    #[serde(default)]
    pub refine_factor: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.search_in_book` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchInBookArgs {
    /// Catalog intake id of the book to confine recall to.
    pub intake_id: i64,
    /// The natural-language query.
    pub query: String,
    /// Maximum number of passages to return.
    pub top_k: Option<usize>,
    /// Force a brute-force scan, ignoring any ANN index.
    #[serde(default)]
    pub bypass_index: bool,
    /// Override the IVF probe count for this query only.
    #[serde(default)]
    pub nprobes: Option<usize>,
    /// Override the IVF-PQ refinement multiplier for this query only.
    #[serde(default)]
    pub refine_factor: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

impl SearchArgs {
    /// Project the override fields onto the underlying [`SearchOptions`]
    /// struct the ops layer consumes.
    fn overrides(&self) -> SearchOptions {
        SearchOptions {
            bypass_index: self.bypass_index,
            nprobes: self.nprobes,
            refine_factor: self.refine_factor,
        }
    }
}

impl SearchInBookArgs {
    /// Project the override fields onto the underlying [`SearchOptions`]
    /// struct the ops layer consumes.
    fn overrides(&self) -> SearchOptions {
        SearchOptions {
            bypass_index: self.bypass_index,
            nprobes: self.nprobes,
            refine_factor: self.refine_factor,
        }
    }
}

/// Arguments for the `library.list_books` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListBooksArgs {
    /// Maximum number of books in this page. Server-side cap applies.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.find_books` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindBooksArgs {
    /// Substring match against the book title.
    #[serde(default)]
    pub title_substring: Option<String>,
    /// Exact-equality match against a contributor name.
    #[serde(default)]
    pub contributor_name: Option<String>,
    /// Restrict the contributor filter to one role (`author` /
    /// `translator` / `editor` / `other`). Only takes effect with
    /// `contributor_name`.
    #[serde(default)]
    pub contributor_role: Option<String>,
    /// Exact-equality match against the file format (`epub`, `pdf`).
    #[serde(default)]
    pub format: Option<String>,
    /// Reserved hook for category-based filtering. Accepted on the
    /// wire today but not honoured server-side; will be enabled in a
    /// future release.
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// Maximum number of books in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.show_book` and `library.show_toc` tools.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BookIdArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Default number of leaves on each side of the anchor when a
/// `library.read_context` call does not specify a radius.
pub const READ_CONTEXT_DEFAULT_RADIUS: u32 = 3;

/// Arguments for the `library.read_context` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadContextArgs {
    /// Corpus node id of the anchor leaf — take it from a search
    /// citation's `start_node_id` or from a passage of a previous
    /// read.
    pub node_id: i64,
    /// Number of leaves to include before the anchor. Defaults to 3;
    /// clamped server-side.
    #[serde(default)]
    pub before: Option<u32>,
    /// Number of leaves to include after the anchor. Defaults to 3;
    /// clamped server-side.
    #[serde(default)]
    pub after: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.read_span` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadSpanArgs {
    /// Corpus node id of the organizing node (chapter, section, ...)
    /// to read — take it from `library.show_toc`.
    pub node_id: i64,
    /// Resume cursor: the `next_offset` of the previous page. Omit to
    /// read from the span's start.
    #[serde(default)]
    pub start_after: Option<i64>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments shared by `library.list_metadata` and
/// `library.list_pending_reviews`. Identical field set — the two tools
/// differ only in which catalog rows they include.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataPageArgs {
    /// Maximum number of rows in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for `library.stats`. Carries only the library selector;
/// the tool itself accepts no other inputs.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct LibraryOnlyArgs {
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for `session.info`. The tool takes no inputs.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SessionInfoArgs {}

/// Response shape returned by `session.info`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionInfoResult {
    /// `bookrack` workspace version, taken from the binary's
    /// `CARGO_PKG_VERSION` at build time.
    pub version: String,
    /// Wall-clock seconds since the daemon started.
    pub uptime_seconds: u64,
    /// Every library registered with the session, sorted by name.
    pub libraries: Vec<String>,
    /// Registry name of the session's default library, or empty when
    /// the daemon was started without a registry-backed selection.
    pub default_library: String,
    /// MCP listener address (`host:port`), or `disabled` when the
    /// daemon is running without `/mcp`.
    pub mcp_addr: String,
    /// Where the library lives on disk (rendered).
    pub data_dir: String,
    /// Ollama HTTP endpoint the daemon will reach.
    pub ollama_url: String,
}

/// Arguments for `session.logs_tail`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SessionLogsTailArgs {
    /// Maximum number of log events to return, from the most recent.
    /// Capped server-side at [`SESSION_LOGS_TAIL_MAX`]. Defaults to
    /// [`SESSION_LOGS_TAIL_DEFAULT`] when omitted.
    #[serde(default)]
    pub n: Option<usize>,
}

/// Default `n` when [`SessionLogsTailArgs::n`] is omitted.
pub const SESSION_LOGS_TAIL_DEFAULT: usize = 100;

/// Server-side cap on `n` for `session.logs_tail`.
pub const SESSION_LOGS_TAIL_MAX: usize = 1024;

/// Response shape returned by `session.logs_tail`: a slice of the
/// daemon's in-memory log ring buffer, oldest first.
#[derive(Debug, Serialize)]
pub struct SessionLogsTailResult {
    /// The events themselves.
    pub events: Vec<LogEvent>,
    /// How many events the tool returned (`<= n` capped by buffer
    /// occupancy).
    pub returned: usize,
}

/// Arguments for `session.queue_status`. The tool takes no inputs.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SessionQueueStatusArgs {}

/// Compact projection of one queued ingest job, used in the `recent`
/// list returned by `session.queue_status`.
#[derive(Debug, Serialize)]
pub struct QueueJobSummary {
    /// UUIDv7 string identifying the job.
    pub id: String,
    /// Lifecycle state, lowercased to match the persisted serde form.
    pub state: String,
    /// Library name the job runs against.
    pub library: String,
    /// Source file path the job ingests.
    pub path: String,
}

/// Response shape returned by `session.queue_status`: counts by state
/// and a small recent-job tail.
#[derive(Debug, Serialize)]
pub struct SessionQueueStatusResult {
    /// Whether the worker is currently paused.
    pub paused: bool,
    /// Jobs in `Pending` state.
    pub pending: usize,
    /// Jobs in `Running` state.
    pub running: usize,
    /// Jobs in `Done` state.
    pub done: usize,
    /// Jobs in `Failed` state.
    pub failed: usize,
    /// Jobs in `Cancelled` state.
    pub cancelled: usize,
    /// The most-recent [`SESSION_QUEUE_STATUS_RECENT`] jobs, newest
    /// first.
    pub recent: Vec<QueueJobSummary>,
}

/// Cap on the `recent` field returned by `session.queue_status`.
pub const SESSION_QUEUE_STATUS_RECENT: usize = 10;

/// Arguments for `session.shutdown`. The tool takes no inputs.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SessionShutdownArgs {}

/// Arguments for `library.metadata.set`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataSetArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field to set. Must be a curator-editable bibliographic
    /// field; an unknown name is rejected and the error carries the
    /// full editable list.
    pub field: String,
    /// The new value.
    pub value: String,
    /// Why this value is correct (e.g. the source consulted). Recorded
    /// on the audit row; required so an LLM edit always carries its
    /// justification.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.clear`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataClearArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field whose override should be removed. A name outside the
    /// editable set is accepted only when an override row with that key
    /// exists (cleanup of stale rows); otherwise it is rejected.
    pub field: String,
    /// Why the override is being removed. Recorded on the audit row;
    /// required so an LLM edit always carries its justification.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.void`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataVoidArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field whose extracted value should be suppressed. Must be a
    /// curator-editable bibliographic field.
    pub field: String,
    /// Why the extracted value is wrong. Recorded on the audit row;
    /// required so an LLM edit always carries its justification.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.ack`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataAckArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Why the gap is being acknowledged; recorded on the audit row.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.approve`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataApproveArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Optional reason recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.reject`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataRejectArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Why the book is being rejected; recorded on the audit row.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// MCP request handler. The streamable-HTTP service clones it per
/// session; the heavy state sits behind an `Arc`, so a clone is cheap
/// and every session shares one warm library registry.
#[derive(Clone)]
pub struct BookrackServer {
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    info_context: LibraryInfoContext,
    started_at: Instant,
    log_stream: LogStreamHandle,
    queue_state: Arc<Mutex<QueueState>>,
    shutdown_tx: broadcast::Sender<()>,
    tool_router: ToolRouter<BookrackServer>,
}

#[tool_router(router = tool_router)]
impl BookrackServer {
    /// Build a handler over the given library registry. `info_context`
    /// carries the static facts (data dir, library name, ollama url,
    /// embed model, MCP address) needed to fill `library.info` and
    /// `session.info` without re-reading the process environment on
    /// every call. `started_at` stamps the daemon's birth so
    /// `session.info` can report uptime. `log_stream` is the shared
    /// in-process log fan-out the `session.logs_tail` tool reads its
    /// ring-buffer snapshot from. `queue_state` is the shared snapshot
    /// of the ingest queue the daemon-REPL drives; the headless mcp
    /// binary passes an inert default since it does not run a queue
    /// worker. `shutdown_tx` carries the session-wide graceful-shutdown
    /// signal so the `session.shutdown` tool can ask the daemon to
    /// stop. Tools route each call through the registry, scoped to
    /// either an explicit `library` selector or the current default
    /// when the selector is absent.
    pub fn new(
        registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
        info_context: LibraryInfoContext,
        started_at: Instant,
        log_stream: LogStreamHandle,
        queue_state: Arc<Mutex<QueueState>>,
        shutdown_tx: broadcast::Sender<()>,
    ) -> BookrackServer {
        BookrackServer {
            registry,
            info_context,
            started_at,
            log_stream,
            queue_state,
            shutdown_tx,
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve a [`LibraryHandle`] for the given selector. Read tools
    /// pass `args.library.as_deref()`; write tools demand a name up
    /// front (their args carry `library: String`, not `Option`) so the
    /// resolution never falls back silently.
    ///
    /// An unknown name produces an MCP `InvalidParams` error carrying
    /// the registry's listed available names — the same diagnostic
    /// shape the CLI surface uses for `--library` typos.
    fn resolve_handle(
        &self,
        library: Option<&str>,
    ) -> Result<Arc<LibraryHandle<OllamaEmbedClient>>, ErrorData> {
        self.registry
            .get(library)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))
    }

    /// Search the library and return cited passages as a JSON array.
    #[tool(
        name = "library.search",
        description = "Search the local book library for passages relevant to a \
                       natural-language query. Returns cited passages, nearest \
                       first, each with a breadcrumb trail and source location."
    )]
    async fn library_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let overrides = args.overrides();
        let hits = reads::search::search(handle.ops(), &args.query, overrides, args.top_k)
            .await
            .map_err(ops_error_to_internal)?;
        tracing::info!(hits = hits.len(), "mcp library.search");
        respond_with(&hits)
    }

    /// Search within one book only — the same ranking as `library.search`
    /// but recall is confined to the chunks owned by `intake_id`.
    #[tool(
        name = "library.search_in_book",
        description = "Search a single book for passages relevant to a query. Pass \
                       the book's intake_id (from library.list_books / \
                       library.show_book). Returns cited passages, nearest first."
    )]
    async fn library_search_in_book(
        &self,
        Parameters(args): Parameters<SearchInBookArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let overrides = args.overrides();
        let result = reads::search::search_in_book(
            handle.ops(),
            args.intake_id,
            &args.query,
            overrides,
            args.top_k,
        )
        .await;
        match result {
            Ok(hits) => {
                tracing::info!(
                    intake_id = args.intake_id,
                    hits = hits.len(),
                    "mcp library.search_in_book"
                );
                respond_with(&hits)
            }
            // Preserve the prior wire shape: an unknown intake reads
            // as an empty hit list on this tool, not a fault.
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Vec<bookrack_ops::Citation>>(&Vec::new())
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Aggregate counts over the library (intakes, book states, retrieval
    /// issues).
    #[tool(
        name = "library.stats",
        description = "Return aggregate counts over the library: intakes by status \
                       and format, book states by pipeline stage, and retrieval \
                       issues by triage status."
    )]
    async fn library_stats(
        &self,
        Parameters(args): Parameters<LibraryOnlyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let stats = reads::books::show_stats(handle.ops()).map_err(ops_error_to_internal)?;
        respond_with(&stats)
    }

    /// List books in the library, paginated.
    #[tool(
        name = "library.list_books",
        description = "List books known to the library, paginated. Returns a slice \
                       of book summaries plus the total matching count and a \
                       truncated flag."
    )]
    async fn library_list_books(
        &self,
        Parameters(args): Parameters<ListBooksArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page =
            reads::books::list_books(handle.ops(), limit, offset).map_err(ops_error_to_internal)?;
        tracing::info!(
            returned = page.books.len(),
            total = page.total,
            "mcp library.list_books"
        );
        respond_with(&page)
    }

    /// Find books by title substring, contributor, format, or status.
    #[tool(
        name = "library.find_books",
        description = "Search the book registry by title substring (fuzzy) and / or \
                       contributor name (exact). `categories` is reserved for a \
                       future release and is ignored today."
    )]
    async fn library_find_books(
        &self,
        Parameters(args): Parameters<FindBooksArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        if let Some(cats) = &args.categories
            && !cats.is_empty()
        {
            tracing::warn!(
                categories = ?cats,
                "library.find_books: categories filter is not yet implemented and was ignored"
            );
        }
        let filter = BookFilter {
            title_substring: args.title_substring,
            contributor_name: args.contributor_name,
            contributor_role: args.contributor_role,
            format: args.format,
            ..BookFilter::default()
        };
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page = reads::books::find_books(handle.ops(), filter, limit, offset)
            .map_err(ops_error_to_internal)?;
        tracing::info!(
            returned = page.books.len(),
            total = page.total,
            "mcp library.find_books"
        );
        respond_with(&page)
    }

    /// Fetch the full bibliographic record for one book.
    #[tool(
        name = "library.show_book",
        description = "Fetch the full bibliographic record for one book by intake id. \
                       Returns effective biblio attributes, the active overrides \
                       (which fields are curated rather than extracted, by whom and \
                       when; `value: null` marks a suppressed extracted value), and \
                       the contributor list, or null when no such book is registered."
    )]
    async fn library_show_book(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::books::show_book(handle.ops(), args.intake_id) {
            Ok(detail) => respond_with(&Some(detail)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<bookrack_ops::dto::BookDetail>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return one book's table of contents.
    #[tool(
        name = "library.show_toc",
        description = "Return the table of contents of one book: a depth-first list \
                       of organizing nodes (chapters, sections, ...) with their \
                       titles, depths, and document-order spans. Returns null when \
                       no such book is ingested."
    )]
    async fn library_show_toc(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::books::show_toc(handle.ops(), args.intake_id) {
            Ok(toc) => respond_with(&Some(toc)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<bookrack_ops::dto::Toc>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Read the leaves around one anchor leaf, in document order.
    #[tool(
        name = "library.read_context",
        description = "Read the passages surrounding one anchor passage, in document \
                       order: N leaves before and N after, paragraph-precise. Pass the \
                       node_id from a search citation's start_node_id or from a \
                       passage of a previous read. Returns null when no such node \
                       exists; rejects organizing nodes (read those with \
                       library.read_span)."
    )]
    async fn library_read_context(
        &self,
        Parameters(args): Parameters<ReadContextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let before = args.before.unwrap_or(READ_CONTEXT_DEFAULT_RADIUS);
        let after = args.after.unwrap_or(READ_CONTEXT_DEFAULT_RADIUS);
        match reads::passages::read_context(handle.ops(), args.node_id, before, after) {
            Ok(window) => respond_with(&Some(window)),
            Err(OpsError::NodeNotFound { .. }) => {
                respond_with::<Option<bookrack_ops::dto::ContextWindow>>(&None)
            }
            Err(e @ OpsError::NotALeaf { .. }) => {
                Err(ErrorData::invalid_params(e.to_string(), None))
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Read one page of an organizing node's span, in document order.
    #[tool(
        name = "library.read_span",
        description = "Read the full text under one TOC node (chapter, section, ...) \
                       in document order, paginated by a character budget. Pass the \
                       node_id from library.show_toc; to continue, pass the returned \
                       next_offset back as start_after until next_offset is null. \
                       Returns null when no such node exists; rejects leaf nodes \
                       (read around those with library.read_context)."
    )]
    async fn library_read_span(
        &self,
        Parameters(args): Parameters<ReadSpanArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::passages::read_span(handle.ops(), args.node_id, args.start_after) {
            Ok(span) => respond_with(&Some(span)),
            Err(OpsError::NodeNotFound { .. }) => {
                respond_with::<Option<bookrack_ops::dto::SpanText>>(&None)
            }
            Err(e @ OpsError::NotOrganizing { .. }) => {
                Err(ErrorData::invalid_params(e.to_string(), None))
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return the metadata-status read for one book.
    #[tool(
        name = "library.show_metadata_audit",
        description = "Return the metadata-status record for one book: the \
                       bibliographic detail plus the persisted audit verdict, \
                       confidence, and current review status. Returns null when \
                       no such book is registered."
    )]
    async fn library_show_metadata_audit(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::metadata::show_metadata_audit(handle.ops(), args.intake_id) {
            Ok(report) => respond_with(&Some(report)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<bookrack_ops::dto::metadata_report::MetadataReport>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return every registered book with its confidence and review
    /// status, unfiltered.
    #[tool(
        name = "library.list_metadata",
        description = "List every registered book with its current confidence and review \
                       status, regardless of audit verdict. Paginated."
    )]
    async fn library_list_metadata(
        &self,
        Parameters(args): Parameters<MetadataPageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page = reads::metadata::list_metadata(handle.ops(), limit, offset)
            .map_err(ops_error_to_internal)?;
        respond_with(&page)
    }

    /// Return books still on the metadata review queue.
    #[tool(
        name = "library.list_pending_reviews",
        description = "List books whose metadata audit confidence is low or medium \
                       and whose review is still pending or acknowledged. Paginated."
    )]
    async fn library_list_pending_reviews(
        &self,
        Parameters(args): Parameters<MetadataPageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page = reads::metadata::list_pending_reviews(handle.ops(), limit, offset)
            .map_err(ops_error_to_internal)?;
        respond_with(&page)
    }

    /// Return the metadata-edit audit trail for one book.
    #[tool(
        name = "library.show_audit_trail",
        description = "Return the metadata-edit audit trail for one book, oldest \
                       first. Returns null when no such book is registered."
    )]
    async fn library_show_audit_trail(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::metadata::show_audit_trail(handle.ops(), args.intake_id) {
            Ok(trail) => respond_with(&Some(trail)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<Vec<bookrack_ops::dto::audit::AuditTrailEntry>>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return the book-level pipeline audit trail for one book.
    #[tool(
        name = "library.show_pipeline_trail",
        description = "Return the book-level pipeline audit trail for one book, \
                       oldest first. Each row records a pipeline sub-step (stage, \
                       outcome, duration, run id). Returns null when no such book \
                       is registered."
    )]
    async fn library_show_pipeline_trail(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::pipeline::show_pipeline_trail(handle.ops(), args.intake_id) {
            Ok(trail) => respond_with(&Some(trail)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<Vec<bookrack_ops::dto::audit::PipelineAuditEntry>>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return the one-page library status card.
    #[tool(
        name = "library.info",
        description = "Return a one-page status card for the open library: schema \
                       versions, embedder configuration, stamped index parameters, \
                       live row count, intake counts, and approximate disk usage."
    )]
    async fn library_info(
        &self,
        Parameters(args): Parameters<LibraryOnlyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let ctx = self.info_context.clone();
        let info = reads::info::show_library_info(handle.ops(), ctx)
            .await
            .map_err(ops_error_to_internal)?;
        respond_with(&info)
    }

    /// Return a one-page status card for the daemon process itself.
    #[tool(
        name = "session.info",
        description = "Daemon runtime summary: bookrack version, uptime, registered \
                       libraries, default library, MCP listener address, data root, \
                       and Ollama endpoint."
    )]
    async fn session_info(
        &self,
        Parameters(_): Parameters<SessionInfoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut libraries: Vec<String> = self
            .registry
            .list()
            .map_err(|e| ErrorData::internal_error(format!("read library registry: {e}"), None))?
            .into_iter()
            .map(|s| s.name)
            .collect();
        libraries.sort();
        let default_library = self.registry.default_name().unwrap_or_default();
        let result = SessionInfoResult {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            libraries,
            default_library,
            mcp_addr: self.info_context.mcp_addr.clone(),
            data_dir: self.info_context.data_dir.clone(),
            ollama_url: self.info_context.ollama_url.clone(),
        };
        respond_with(&result)
    }

    /// Return the most recent log events from the daemon's in-memory
    /// ring buffer. Hands clients a one-shot snapshot; the live
    /// SSE endpoint at `/session/logs` is the streaming counterpart.
    #[tool(
        name = "session.logs_tail",
        description = "Return the most recent N log events from the daemon's in-memory \
                       ring buffer (oldest first within the returned slice). N defaults \
                       to 100 and is capped server-side at 1024."
    )]
    async fn session_logs_tail(
        &self,
        Parameters(args): Parameters<SessionLogsTailArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let n = args
            .n
            .unwrap_or(SESSION_LOGS_TAIL_DEFAULT)
            .min(SESSION_LOGS_TAIL_MAX);
        let events = self.log_stream.tail(n);
        let returned = events.len();
        respond_with(&SessionLogsTailResult { events, returned })
    }

    /// Snapshot the daemon-REPL's ingest queue: counts by lifecycle
    /// state plus the most recent jobs.
    ///
    /// The headless `bookrack-mcp` binary does not drive a queue
    /// worker; against it this tool reports an inert empty state.
    #[tool(
        name = "session.queue_status",
        description = "Snapshot of the ingest queue: counts by lifecycle state plus the \
                       most recent jobs (newest first, capped at 10). Returns an empty \
                       snapshot when run against a daemon that does not host a queue worker."
    )]
    async fn session_queue_status(
        &self,
        Parameters(_): Parameters<SessionQueueStatusArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self
            .queue_state
            .lock()
            .map_err(|e| ErrorData::internal_error(format!("queue state lock: {e}"), None))?;
        let mut counts = [0usize; 5];
        for job in &state.jobs {
            let idx = match job.state {
                JobState::Pending => 0,
                JobState::Running => 1,
                JobState::Done => 2,
                JobState::Failed => 3,
                JobState::Cancelled => 4,
            };
            counts[idx] += 1;
        }
        let recent: Vec<QueueJobSummary> = state
            .jobs
            .iter()
            .rev()
            .take(SESSION_QUEUE_STATUS_RECENT)
            .map(|j| QueueJobSummary {
                id: j.id.clone(),
                state: format!("{:?}", j.state).to_ascii_lowercase(),
                library: j.library.clone(),
                path: j.path.display().to_string(),
            })
            .collect();
        let result = SessionQueueStatusResult {
            paused: state.paused,
            pending: counts[0],
            running: counts[1],
            done: counts[2],
            failed: counts[3],
            cancelled: counts[4],
            recent,
        };
        respond_with(&result)
    }

    /// Ask the daemon to perform a graceful shutdown. Fires the
    /// session-wide broadcast signal that the signal listener, REPL,
    /// queue worker, and MCP listener all subscribe to; the daemon's
    /// own join logic then closes everything in order.
    ///
    /// The tool returns immediately once the signal is sent — the
    /// shutdown itself happens asynchronously in the daemon's main
    /// loop, and the calling client sees the connection close as the
    /// listener winds down.
    #[tool(
        name = "session.shutdown",
        description = "Ask the daemon to perform a graceful shutdown. Returns immediately \
                       after firing the shutdown signal; the listener and queue worker \
                       then wind down asynchronously."
    )]
    async fn session_shutdown(
        &self,
        Parameters(_): Parameters<SessionShutdownArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let _ = self.shutdown_tx.send(());
        respond_with(&serde_json::json!({"status": "shutting down"}))
    }

    /// Snapshot the vector store.
    #[tool(
        name = "library.vectors_status",
        description = "Snapshot the vector store: chunk-table row count, every ANN \
                       index LanceDB enumerates with its per-shard statistics, the \
                       persisted ANN config, and any drift between the on-disk meta \
                       and the indices LanceDB actually carries."
    )]
    async fn library_vectors_status(
        &self,
        Parameters(args): Parameters<LibraryOnlyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let status = reads::vectors::status(handle.ops())
            .await
            .map_err(ops_error_to_internal)?;
        respond_with(&status)
    }

    /// Set an override on one bibliographic field of a book.
    #[tool(
        name = "library.metadata.set",
        description = "Set an override on one editable bibliographic field of one \
                       book. An unknown field name is rejected with the editable \
                       list in the error. The extracted value is preserved; the \
                       override wins on read. Appends one audit row tagged \
                       `actor_kind=llm` carrying the required `reason`."
    )]
    async fn library_metadata_set(
        &self,
        Parameters(args): Parameters<MetadataSetArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::SetMetadataFieldRequest {
            intake_id: args.intake_id,
            field: args.field,
            value: args.value,
            reason: Some(args.reason),
        };
        let outcome = writes::metadata::set_metadata_field(handle.ops(), req)
            .map_err(ops_error_to_edit_error)?;
        respond_with(&outcome)
    }

    /// Remove an override on one bibliographic field, reverting to the
    /// extracted value.
    #[tool(
        name = "library.metadata.clear",
        description = "Remove an override on one bibliographic field of one book, \
                       reverting to the extracted value. An editable field with no \
                       override still appends an audit row recording the attempt; \
                       an unknown field name is rejected unless a stale override \
                       row with that key exists. The required `reason` lands on \
                       the audit row."
    )]
    async fn library_metadata_clear(
        &self,
        Parameters(args): Parameters<MetadataClearArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::ClearMetadataFieldRequest {
            intake_id: args.intake_id,
            field: args.field,
            reason: Some(args.reason),
        };
        let outcome = writes::metadata::clear_metadata_field(handle.ops(), req)
            .map_err(ops_error_to_edit_error)?;
        respond_with(&outcome)
    }

    /// Suppress one field's extracted value with a NULL override.
    #[tool(
        name = "library.metadata.void",
        description = "Suppress one field's extracted value without supplying a \
                       replacement: the field reads as absent until a correct \
                       value is set. For extracted values known to be wrong when \
                       no right value is at hand. `library.metadata.clear` \
                       removes the suppression. Appends one audit row tagged \
                       `actor_kind=llm` carrying the required `reason`."
    )]
    async fn library_metadata_void(
        &self,
        Parameters(args): Parameters<MetadataVoidArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::VoidMetadataFieldRequest {
            intake_id: args.intake_id,
            field: args.field,
            reason: Some(args.reason),
        };
        let outcome = writes::metadata::void_metadata_field(handle.ops(), req)
            .map_err(ops_error_to_edit_error)?;
        respond_with(&outcome)
    }

    /// Acknowledge a metadata gap: flip the review row to `acknowledged`
    /// with a recorded reason.
    #[tool(
        name = "library.metadata.ack",
        description = "Acknowledge a metadata gap on one book: the audit verdict \
                       is unchanged; the review row is flipped to `acknowledged` \
                       with the given reason. The book stays on the review queue \
                       until approved or rejected."
    )]
    async fn library_metadata_ack(
        &self,
        Parameters(args): Parameters<MetadataAckArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::AcknowledgeMetadataGapRequest {
            intake_id: args.intake_id,
            reason: args.reason,
        };
        let outcome = writes::metadata::acknowledge_metadata_gap(handle.ops(), req)
            .map_err(ops_error_to_internal)?;
        respond_with(&outcome)
    }

    /// Approve the metadata record: flip the review row to `approved`.
    #[tool(
        name = "library.metadata.approve",
        description = "Approve the metadata record on one book: assert that the \
                       effective metadata matches the source. Flips the review \
                       row to `approved`. The audit verdict is unchanged."
    )]
    async fn library_metadata_approve(
        &self,
        Parameters(args): Parameters<MetadataApproveArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::ApproveMetadataRequest {
            intake_id: args.intake_id,
            reason: args.reason,
        };
        let outcome =
            writes::metadata::approve_metadata(handle.ops(), req).map_err(ops_error_to_internal)?;
        respond_with(&outcome)
    }

    /// Reject the book: flip the review row to `rejected`.
    #[tool(
        name = "library.metadata.reject",
        description = "Reject one book: flip the review row to `rejected`. \
                       Pipeline rows stay in place so downstream consumers can \
                       filter on `rejected`. Records the reason on the audit row."
    )]
    async fn library_metadata_reject(
        &self,
        Parameters(args): Parameters<MetadataRejectArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::RejectMetadataRequest {
            intake_id: args.intake_id,
            reason: args.reason,
        };
        let outcome =
            writes::metadata::reject_metadata(handle.ops(), req).map_err(ops_error_to_internal)?;
        respond_with(&outcome)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BookrackServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Search and browse a local, offline library of books. Tools: \
             `library.stats` (counts), `library.list_books` / `library.find_books` \
             (browse and search the registry), `library.show_book` / `library.show_toc` \
             (per-book metadata and table of contents), `library.search` (vector \
             search across the whole library), `library.search_in_book` (vector \
             search confined to one book)."
                .to_string(),
        )
    }

    /// Override the `#[tool_handler]`-generated dispatch so every tool
    /// call runs attributed to [`Caller::mcp`], regardless of the
    /// [`Caller`] baked into the shared [`Ops`] handle. The daemon
    /// builds one [`Ops`] tagged with its launch surface
    /// (`Caller::cli()` / `Caller::gui()`) and shares it across REPL,
    /// HTTP, and queue worker; without this wrap, both the recorded
    /// tool-call `source` and the `actor_kind` / `actor_detail` on
    /// write audit rows would carry the host surface's identity
    /// instead of `llm` / `mcp`.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        with_caller_override(Caller::mcp(), self.tool_router.call(tcc)).await
    }
}

/// Encode `value` to a JSON string and wrap it as the body of a successful
/// tool response. Centralises serialization so every tool returns the same
/// `text` content shape.
fn respond_with<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json =
        serde_json::to_string(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Map a generic [`OpsError`] to an MCP internal error.
fn ops_error_to_internal(e: OpsError) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// Map an [`OpsError`] from a metadata field edit to an MCP error:
/// a rejected field name is the caller's input problem, so it surfaces
/// as `invalid_params` (with the editable list in the message) rather
/// than an internal error.
fn ops_error_to_edit_error(e: OpsError) -> ErrorData {
    match &e {
        OpsError::UnknownMetadataField { .. } => ErrorData::invalid_params(e.to_string(), None),
        _ => ops_error_to_internal(e),
    }
}

/// Bind the streamable-HTTP server at `addr` and serve until the
/// shutdown channel fires.
///
/// Two HTTP routes are mounted:
///
/// * `/mcp` — the MCP streamable-HTTP service. Connect an MCP client
///   to `http://<addr>/mcp`. Every tool resolves its target library
///   through the registry — by the `library` selector when present,
///   by the registry's current default when absent. Write tools
///   require an explicit `library` name in their input; read tools
///   accept `library` as optional.
/// * `/session/logs` — a Server-Sent Events endpoint that streams
///   `LogEvent`s from `log_stream` as soon as they are produced. Each
///   SSE frame's `data:` payload is one log event serialised as JSON;
///   the stream stays open until the client disconnects or the
///   daemon shuts down.
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
/// daemon-REPL drives; the headless `bookrack-mcp` binary passes an
/// inert default since it does not run a queue worker.
///
/// `shutdown_tx` is the session-wide graceful-shutdown broadcaster
/// that the signal listener, REPL, queue worker, and this listener
/// all subscribe to. The `session.shutdown` MCP tool calls `send` on
/// it; this function also subscribes through `shutdown_rx` (the
/// receiver side of the same broadcast) for its own listener loop.
// Eight separate handles are the natural shape here: each is owned by a
// distinct subsystem (registry, info, log fan-out, queue, shutdown
// fan-in/out, listener address) that the caller threaded through its
// own state machine. Bundling them into a struct buys nothing — the
// only two call sites are already passing each handle exactly once —
// and would just trade names for repetition.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    info_context: LibraryInfoContext,
    started_at: Instant,
    log_stream: LogStreamHandle,
    queue_state: Arc<Mutex<QueueState>>,
    shutdown_tx: broadcast::Sender<()>,
    addr: &str,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let log_stream_for_sse = log_stream.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(BookrackServer::new(
                registry.clone(),
                info_context.clone(),
                started_at,
                log_stream.clone(),
                queue_state.clone(),
                shutdown_tx.clone(),
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
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind MCP server to {addr}"))?;
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
/// [`DaemonRuntime`](bookrack_runtime::DaemonRuntime). Returns `None`
/// when the runtime came up with MCP disabled (`mcp_label ==
/// "disabled"`), so the daemon runs without an HTTP surface. Shared by
/// every daemon host (CLI, GUI) so the listener wiring has one source.
pub fn spawn_listener(
    runtime: &bookrack_runtime::DaemonRuntime,
) -> Option<tokio::task::JoinHandle<anyhow::Result<()>>> {
    if runtime.mcp_label == "disabled" {
        tracing::info!("MCP listener disabled (--no-mcp); session running without /mcp");
        return None;
    }
    let registry = Arc::clone(&runtime.registry);
    let info_context = runtime.info_context.clone();
    let started_at = runtime.started_at;
    let log_stream = runtime.log_stream.clone();
    let queue_state = Arc::clone(&runtime.queue_state);
    let shutdown_tx = runtime.shutdown_tx.clone();
    let addr = runtime.mcp_label.clone();
    let rx = runtime.shutdown_tx.subscribe();
    Some(tokio::spawn(async move {
        serve(
            registry,
            info_context,
            started_at,
            log_stream,
            queue_state,
            shutdown_tx,
            &addr,
            rx,
        )
        .await
    }))
}

/// Enumerate every MCP tool the live server exposes. Calls into the
/// static [`BookrackServer::tool_router`] generated by rmcp's
/// `#[tool_router]` macro, so the list stays in lockstep with the
/// `#[tool]` annotations above without a separate registry.
///
/// The daemon's control plane reaches for this list at startup so
/// `daemon.mcp_tools` can answer without spinning up an MCP transport.
pub fn list_tools() -> Vec<bookrack_runtime::control::methods::meta::McpToolInfo> {
    BookrackServer::tool_router()
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
    use bookrack_ops::Citation;
    use bookrack_ops::dto::{
        BookDetail, BookSummary, ContextWindow, ContributorEntry, LibraryStats, ListBooksResult,
        Passage, SpanText, Toc, TocNode,
    };
    use bookrack_query::NodeId;

    fn citation(node: i64) -> Citation {
        Citation {
            text: "passage".to_string(),
            breadcrumb: "A Test Book \u{203a} Chapter One".to_string(),
            intake_id: NodeId::new(node).partition().get(),
            toc_position: Some(0),
            enclosing_node_id: Some(NodeId::new(node).partition().root()),
            start_node_id: NodeId::new(node),
            start_char_offset: 0,
            end_node_id: NodeId::new(node),
            end_char_offset: 7,
            norm_chunk_sha256: "sha".to_string(),
            distance: 0.1,
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
        };
        let value = serde_json::to_value(&summary).expect("serialize");
        assert_eq!(value["intake_id"], 1);
        assert_eq!(value["title"], "A Title");
        assert_eq!(value["status"], "extracted");
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
            effective_biblio: biblio,
            overrides: Vec::new(),
            contributors: vec![ContributorEntry {
                role: "author".to_string(),
                ordinal: 0,
                name: "An Author".to_string(),
                nationality: None,
                origin: "extracted".to_string(),
            }],
        };
        let value = serde_json::to_value(&detail).expect("serialize");
        assert_eq!(value["effective_biblio"]["title"], "T");
        assert_eq!(value["contributors"][0]["role"], "author");
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
    fn toc_serializes_with_a_truncated_flag() {
        let toc = Toc {
            intake_id: 1,
            nodes: vec![TocNode {
                node_id: 100_000_001,
                parent_id: None,
                title: Some("Root".to_string()),
                depth: 0,
                ordinal: 0,
                toc_lo: Some(1),
                toc_hi: Some(50),
            }],
            truncated: false,
        };
        let value = serde_json::to_value(&toc).expect("serialize");
        assert_eq!(value["intake_id"], 1);
        assert_eq!(value["nodes"][0]["node_id"], 100_000_001);
        assert_eq!(value["truncated"], false);
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
}
