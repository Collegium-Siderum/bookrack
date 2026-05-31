// SPDX-License-Identifier: Apache-2.0

//! bookrack command-line entry point.
//!
//! A thin shell over the library pipeline: parse arguments, resolve
//! [`Config`], install the tracing subscriber, open the stores, and call
//! the graduated `ingest_book` / `search` entry points. All orchestration
//! lives in those library functions; this binary only wires inputs to them
//! and renders their reports. Operational tuning comes entirely from the
//! environment via `Config` and the `*Config::from_env` resolvers — the
//! command surface carries no tuning flags, so there is a single source of
//! truth for every default.

mod render;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig, LibrarySelection, LogConfig, SearchConfig};
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ingest::{IngestParams, ingest_book};
use bookrack_search::search;
use bookrack_vectors::ChunkStore;

#[derive(clap::Parser)]
#[command(name = "bookrack", version, about = "Search a local library of books.")]
struct Cli {
    /// Operate on the library at this data root, overriding the
    /// environment. Mutually exclusive with `--library`.
    #[arg(long, global = true, conflicts_with = "library")]
    data_dir: Option<PathBuf>,
    /// Operate on the named library from the registry (see
    /// BOOKRACK_REGISTRY). Mutually exclusive with `--data-dir`.
    #[arg(long, global = true)]
    library: Option<String>,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    /// The library selection these top-level flags express.
    fn selection(&self) -> LibrarySelection {
        LibrarySelection {
            data_dir: self.data_dir.clone(),
            library: self.library.clone(),
        }
    }
}

#[derive(clap::Subcommand)]
enum Command {
    /// Ingest and embed a single file into the library.
    Ingest {
        /// Path to the source file to ingest.
        path: PathBuf,
    },
    /// Query the library and print cited passages.
    Query {
        /// The natural-language query.
        text: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    let cfg = Config::resolve(&cli.selection()).context("resolve configuration")?;
    let _guard = bookrack_obs::init(&cfg, &LogConfig::from_env());

    match cli.command {
        Command::Ingest { path } => run_ingest(&cfg, &path).await,
        Command::Query { text } => run_query(&cfg, &text).await,
    }
}

/// Build the embedding client from the environment-resolved knobs.
fn embedder(cfg: &Config, embed_cfg: &EmbedConfig) -> Result<OllamaEmbedClient> {
    OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build embedding client")
}

async fn run_ingest(cfg: &Config, path: &Path) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let embedder = embedder(cfg, &embed_cfg)?;
    let params = IngestParams {
        embed: embed_cfg,
        ..Default::default()
    };
    let report = ingest_book(
        path,
        &mut corpus,
        &mut catalog,
        &cfg.lancedb_dir(),
        &embedder,
        &params,
    )
    .await
    .context("ingest book")?;
    render::ingest(&report);
    Ok(())
}

async fn run_query(cfg: &Config, text: &str) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let search_cfg = SearchConfig::from_env();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let embedder = embedder(cfg, &embed_cfg)?;

    // The store's vector width is fixed at creation and must match the
    // model. Probe the embedder once to learn it before reopening.
    let probe = embedder
        .embed_batch(&["dimension probe".to_string()])
        .await
        .context("probe embedding dimension")?;
    let dim = probe
        .first()
        .map(Vec::len)
        .context("embedder returned no vector")?;

    let store = ChunkStore::open(&cfg.lancedb_dir(), dim)
        .await
        .context("open vector store")?;
    // Refuse to serve an index built with a different model or a stale
    // algorithm version; an empty index has no provenance to check.
    if store.count_rows().await.context("count vector rows")? > 0 {
        corpus
            .verify_index_stamps(&bookrack_ingest::current_index_stamps(
                &embed_cfg.model,
                dim as u32,
            ))
            .context("verify index stamps")?;
    }
    let hits = search(text, &corpus, &store, &embedder, search_cfg.top_k)
        .await
        .context("run query")?;
    render::citations(&hits);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn data_dir_and_library_are_mutually_exclusive() {
        let parsed = Cli::try_parse_from([
            "bookrack",
            "--data-dir",
            "/x",
            "--library",
            "test",
            "query",
            "q",
        ]);
        assert!(parsed.is_err(), "the two selectors must not be combined");
    }

    #[test]
    fn selection_carries_the_flags_through() {
        let cli = Cli::try_parse_from(["bookrack", "--library", "test", "query", "q"])
            .expect("a lone --library parses");
        let selection = cli.selection();
        assert_eq!(selection.library.as_deref(), Some("test"));
        assert!(selection.data_dir.is_none());
    }
}
