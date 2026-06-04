// SPDX-License-Identifier: Apache-2.0

//! bookrack MCP server: exposes the read-only query facade to agent
//! clients over streamable HTTP.
//!
//! The server is a thin shell. It holds one warm [`Library`] behind an
//! `Arc` and maps each tool call onto a facade method; it depends only on
//! `bookrack-query`, never on the database crates behind that facade, so a
//! schema change downstream leaves this crate untouched.

use std::sync::Arc;

use anyhow::Context;
use bookrack_embed::OllamaEmbedClient;
use bookrack_query::Library;
use bookrack_query::dto::BookFilter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

/// The warm query state, shared across MCP sessions.
type SharedLibrary = Arc<Library<OllamaEmbedClient>>;

/// Arguments for the `library.search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// The natural-language query to search the library for.
    pub query: String,
    /// Maximum number of passages to return. Defaults to the server's
    /// configured top-k when omitted.
    pub top_k: Option<usize>,
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
}

/// Arguments for the `library.show_book` and `library.show_toc` tools.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BookIdArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
}

/// MCP request handler. The streamable-HTTP service clones it per session;
/// the heavy state sits behind an `Arc`, so a clone is cheap and every
/// session shares one warm library.
#[derive(Clone)]
pub struct BookrackServer {
    library: SharedLibrary,
    tool_router: ToolRouter<BookrackServer>,
}

#[tool_router(router = tool_router)]
impl BookrackServer {
    /// Build a handler over the given warm library.
    pub fn new(library: SharedLibrary) -> BookrackServer {
        BookrackServer {
            library,
            tool_router: Self::tool_router(),
        }
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
        let hits = self
            .library
            .search(&args.query, args.top_k)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
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
        let hits = self
            .library
            .search_in_book(args.intake_id, &args.query, args.top_k)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        tracing::info!(
            intake_id = args.intake_id,
            hits = hits.len(),
            "mcp library.search_in_book"
        );
        respond_with(&hits)
    }

    /// Aggregate counts over the library (intakes, book states, retrieval
    /// issues).
    #[tool(
        name = "library.stats",
        description = "Return aggregate counts over the library: intakes by status \
                       and format, book states by pipeline stage, and retrieval \
                       issues by triage status."
    )]
    async fn library_stats(&self) -> Result<CallToolResult, ErrorData> {
        let stats = self
            .library
            .stats()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
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
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page = self
            .library
            .list_books(limit, offset)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
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
        let page = self
            .library
            .find_books(filter, limit, offset)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
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
                       Returns effective biblio attributes and the contributor list, \
                       or null when no such book is registered."
    )]
    async fn library_show_book(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let detail = self
            .library
            .show_book(args.intake_id)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        respond_with(&detail)
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
        let toc = self
            .library
            .show_toc(args.intake_id)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        respond_with(&toc)
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
}

/// Encode `value` to a JSON string and wrap it as the body of a successful
/// tool response. Centralises serialization so every tool returns the same
/// `text` content shape.
fn respond_with<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json =
        serde_json::to_string(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Bind the streamable-HTTP server at `addr` and serve until Ctrl-C.
///
/// The MCP service is mounted at `/mcp`; connect a client as an HTTP MCP
/// server pointed at `http://<addr>/mcp`.
pub async fn serve(library: SharedLibrary, addr: &str) -> anyhow::Result<()> {
    let service = StreamableHttpService::new(
        move || Ok(BookrackServer::new(library.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind MCP server to {addr}"))?;
    tracing::info!(%addr, "bookrack MCP server listening on /mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serve MCP server")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bookrack_query::dto::{
        BookDetail, BookSummary, ContributorEntry, LibraryStats, ListBooksResult, Toc, TocNode,
    };
    use bookrack_query::{Citation, NodeId};

    fn citation(node: i64) -> Citation {
        Citation {
            text: "passage".to_string(),
            breadcrumb: "A Test Book \u{203a} Chapter One".to_string(),
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
}
