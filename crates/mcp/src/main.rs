// SPDX-License-Identifier: Apache-2.0

//! bookrack MCP daemon entry point.
//!
//! Resolve configuration, install the tracing subscriber, build the
//! embedding client and warm query library, then serve the MCP protocol
//! over streamable HTTP until Ctrl-C. The heavy startup cost — probing the
//! embedding dimension and opening the vector store — is paid once here, so
//! a client connection is a cheap HTTP handshake rather than a cold start.
//!
//! The daemon serves one library for its lifetime; switching libraries
//! means restarting with a different `--data-dir` / `--library`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, EmbedConfig, LibrarySelection, LogConfig, McpConfig, ResolutionSource, SearchConfig,
};
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use bookrack_query::Library;

#[derive(clap::Parser)]
#[command(
    name = "bookrack-mcp",
    version,
    about = "Serve a local library over MCP."
)]
struct Cli {
    /// Serve the library at this data root, overriding the environment.
    /// Mutually exclusive with `--library`.
    #[arg(long, conflicts_with = "library")]
    data_dir: Option<PathBuf>,
    /// Serve the named library from the registry (see BOOKRACK_REGISTRY).
    /// Mutually exclusive with `--data-dir`.
    #[arg(long)]
    library: Option<String>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    let selection = LibrarySelection {
        data_dir: cli.data_dir,
        library: cli.library,
    };
    let cfg = Config::resolve(&selection).context("resolve configuration")?;
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

    // Reject a catalog the binary cannot serve before the listener binds.
    // Lazy opens inside tool handlers used to let the daemon accept
    // connections and then fail every catalog-touching call with
    // SchemaTooNew or ReaderTooOld; this preflight surfaces the mismatch
    // at startup instead.
    let catalog_db = cfg.catalog_db();
    if catalog_db.exists() {
        Catalog::open_read_only(&catalog_db).context("preflight catalog schema check failed")?;
    }

    let search_cfg = SearchConfig::from_env();
    let library = Library::open(
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        embedder,
        embed_cfg.model.clone(),
        search_cfg.top_k,
    )
    .await
    .context("open query library")?;

    let ops = Ops::with_library(
        library,
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        Caller::mcp(),
    );

    let info_context = LibraryInfoContext {
        data_dir: cfg.data_dir().display().to_string(),
        library_name: cfg.library().map(str::to_string),
        resolution_source: resolution_source_label(cfg.source()).to_string(),
        ollama_url: cfg.ollama_url().to_string(),
        embed_model_configured: embed_cfg.model.clone(),
    };

    // Wrap the single warm Ops in a one-element LibraryRegistry. The
    // registry routes every later phase's call (REPL, queue worker,
    // future MCP per-tool library selector) through one chokepoint;
    // this binary still serves exactly one library, but its plumbing
    // now matches the multi-library shape.
    let library_name = cfg.library().unwrap_or("default").to_string();
    let handle = LibraryHandle::new(library_name, ops);
    let registry = LibraryRegistry::single(handle);

    let mcp_cfg = McpConfig::from_env();
    bookrack_mcp::serve(registry, info_context, &mcp_cfg.addr).await
}

fn resolution_source_label(source: ResolutionSource) -> &'static str {
    match source {
        ResolutionSource::DataDirFlag => "--data-dir flag",
        ResolutionSource::LibraryFlag => "--library flag",
        ResolutionSource::EnvVar => "BOOKRACK_DATA_DIR env",
        ResolutionSource::PortableExeNeighbor => "portable layout",
        ResolutionSource::RegistryDefault => "registry default",
        ResolutionSource::DefaultRegistryDefault => "default registry default",
        ResolutionSource::Explicit => "explicit",
    }
}
