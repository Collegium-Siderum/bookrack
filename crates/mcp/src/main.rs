// SPDX-License-Identifier: Apache-2.0

//! bookrack MCP daemon entry point.
//!
//! Resolve configuration, install the tracing subscriber, build the
//! embedding client and warm query library, then serve the MCP protocol
//! over streamable HTTP until Ctrl-C. The heavy startup cost — probing the
//! embedding dimension and opening the vector store — is paid once here, so
//! a client connection is a cheap HTTP handshake rather than a cold start.

use std::sync::Arc;

use anyhow::{Context, Result};
use bookrack_config::{Config, EmbedConfig, LogConfig, McpConfig, SearchConfig};
use bookrack_embed::OllamaEmbedClient;
use bookrack_query::Library;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::load().context("resolve configuration")?;
    let _guard = bookrack_obs::init(&cfg, &LogConfig::from_env());

    let embed_cfg = EmbedConfig::from_env();
    let embedder = OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build embedding client")?;

    let search_cfg = SearchConfig::from_env();
    let library = Library::open(
        cfg.corpus_db(),
        &cfg.lancedb_dir(),
        embedder,
        search_cfg.top_k,
    )
    .await
    .context("open query library")?;

    let mcp_cfg = McpConfig::from_env();
    bookrack_mcp::serve(Arc::new(library), &mcp_cfg.addr).await
}
