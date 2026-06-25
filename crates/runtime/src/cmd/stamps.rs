// SPDX-License-Identifier: Apache-2.0

//! `bookrack stamps reconcile` — write the binary's current index
//! stamps onto the corpus, useful after a model swap.

use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;
use eyre::{Context, ContextCompat, Result};

use crate::embed_helpers::embedder;

pub async fn reconcile(cfg: &Config) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let embedder = embedder(cfg, &embed_cfg)?;
    // Probe the embedder once for its current vector dimension. The
    // probe is the only network call this command makes; the corpus
    // write happens locally.
    let probe = embedder
        .embed_batch(&["dimension probe".to_string()])
        .await
        .context("probe embedding dimension")?;
    let dim = probe
        .first()
        .map(Vec::len)
        .context("embedder returned no vector")?;
    let stamps = bookrack_ingest::current_index_stamps(&embed_cfg.model, dim as u32);
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    corpus
        .reconcile_index_stamps(&stamps)
        .context("reconcile index stamps")?;
    println!(
        "stamps reconciled: embed_model={} vector_dim={} chunk_version={} normalize_version={}",
        stamps.embed_model, stamps.vector_dim, stamps.chunk_version, stamps.normalize_version,
    );
    Ok(())
}
