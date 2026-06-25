// SPDX-License-Identifier: Apache-2.0

//! `bookrack papers stamps reconcile` — write the live embedder's model
//! name and vector dimension into `papers_corpus.db`'s `index_meta`
//! table. Peer of [`crate::cmd::stamps`] for the paper pipeline.

use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;
use eyre::{Context, ContextCompat, Result};

use crate::embed_helpers::embedder;

pub async fn reconcile(cfg: &Config) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let embedder = embedder(cfg, &embed_cfg)?;
    let probe = embedder
        .embed_batch(&["dimension probe".to_string()])
        .await
        .context("probe papers embedding dimension")?;
    let dim = probe
        .first()
        .map(Vec::len)
        .context("embedder returned no vector")?;
    let stamps = bookrack_glean::stamps::current_index_stamps(&embed_cfg.model, dim as u32);
    let corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    corpus
        .reconcile_index_stamps(&stamps)
        .context("reconcile papers index stamps")?;
    println!(
        "papers stamps reconciled: embed_model={} vector_dim={} chunk_version={} normalize_version={}",
        stamps.embed_model, stamps.vector_dim, stamps.chunk_version, stamps.normalize_version,
    );
    Ok(())
}
