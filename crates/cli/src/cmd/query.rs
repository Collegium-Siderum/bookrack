// SPDX-License-Identifier: Apache-2.0

//! `bookrack query` — run a search against the warm `Library` and
//! render the cited passages.

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig, SearchConfig};
use bookrack_ops::reads;
use bookrack_ops::{Caller, Ops, OpsError, SearchOptions};
use bookrack_query::Library;

use crate::embed_helpers::embedder;
use crate::render;
use crate::util::truncate_query_with_warning;

pub async fn run(
    cfg: &Config,
    text: &str,
    in_book: Option<i64>,
    bypass_ann: bool,
    nprobes: Option<usize>,
    refine_factor: Option<u32>,
) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let search_cfg = SearchConfig::from_env();
    if search_cfg.top_k == 0 {
        anyhow::bail!(
            "BOOKRACK_SEARCH_TOP_K must be at least 1; got 0 (queries return no rows when top_k is 0)"
        );
    }
    let owned_text = truncate_query_with_warning(text);
    let text = owned_text.as_str();
    // Refuse a `--in-book` against an unknown or already-removed intake
    // up front, before the embedder probe and the vector store open.
    // Without this guard the query silently returns zero hits and reads
    // as "this book is fine, it just has no matches" — which is the
    // opposite of what happened.
    if let Some(intake_id) = in_book {
        let catalog = Catalog::open(&cfg.catalog_db()).context("open catalog")?;
        if catalog
            .intake_by_id(intake_id)
            .context("look up intake")?
            .is_none()
        {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
    }
    let embedder = embedder(cfg, &embed_cfg)?;

    // The query facade probes the embedder for its vector width, opens
    // the chunk store at that dimension, and verifies the index stamps
    // against this binary when the store is non-empty.
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
        cfg.books_dir(),
        cfg.backup_dir(),
        Caller::cli(),
    );

    // CLI flags win over env, which wins over meta defaults inside
    // retrieve_with.
    let env = bookrack_search::env_overrides();
    let overrides = SearchOptions {
        bypass_index: bypass_ann || env.bypass_index,
        nprobes: nprobes.or(env.nprobes),
        refine_factor: refine_factor.or(env.refine_factor),
    };
    let hits = match in_book {
        Some(intake_id) => {
            match reads::search::search_in_book(&ops, intake_id, text, overrides, None).await {
                Ok(hits) => hits,
                // The pre-check above already refused unknown intakes;
                // a races-with-remove path falls through to the same
                // diagnostic instead of an anyhow chain.
                Err(OpsError::IntakeNotFound { intake_id }) => {
                    anyhow::bail!("no intake registered for book {intake_id}");
                }
                Err(e) => return Err(anyhow::Error::from(e).context("run query in book")),
            }
        }
        None => reads::search::search(&ops, text, overrides, None)
            .await
            .context("run query")?,
    };
    render::citations(&hits, search_cfg.weak_distance_threshold);
    Ok(())
}
