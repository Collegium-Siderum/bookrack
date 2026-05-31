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
use bookrack_query::{Citation, Library};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

/// The warm query state, shared across MCP sessions.
type SharedLibrary = Arc<Library<OllamaEmbedClient>>;

/// Arguments for the `search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// The natural-language query to search the library for.
    pub query: String,
    /// Maximum number of passages to return. Defaults to the server's
    /// configured top-k when omitted.
    pub top_k: Option<usize>,
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
        description = "Search the local book library for passages relevant to a \
                       natural-language query. Returns cited passages, nearest \
                       first, each with a breadcrumb trail and source location."
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let hits = self
            .library
            .search(&args.query, args.top_k)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        tracing::info!(hits = hits.len(), "mcp search");
        let json =
            citations_json(&hits).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BookrackServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Search a local, offline library of books. Call the `search` tool with a \
             natural-language query to retrieve cited passages."
                .to_string(),
        )
    }
}

/// Serialize search results to a JSON array string. The passage DTO owns
/// its field shape; this never restates it, so a new field flows through
/// automatically.
fn citations_json(hits: &[Citation]) -> serde_json::Result<String> {
    serde_json::to_string(hits)
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
    use super::*;
    use bookrack_query::NodeId;

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
    fn citations_json_is_an_array_with_bare_int_node_ids() {
        let json = citations_json(&[citation(100_000_001)]).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(value.is_array());
        assert_eq!(value[0]["start_node_id"], serde_json::json!(100_000_001));
        assert_eq!(value[0]["breadcrumb"], "A Test Book \u{203a} Chapter One");
    }

    #[test]
    fn citations_json_of_no_hits_is_an_empty_array() {
        assert_eq!(citations_json(&[]).expect("serialize"), "[]");
    }
}
