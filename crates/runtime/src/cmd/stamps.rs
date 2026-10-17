// SPDX-License-Identifier: Apache-2.0

//! `bookrack stamps reconcile` — write the binary's current index
//! stamps onto the corpus, useful after a model swap.

use bookrack_config::Config;
use bookrack_corpus::{Corpus, IndexStamps};
use eyre::{Context, ContextCompat, Result};
use serde::Serialize;

use crate::embed_helpers::embedder;

/// The index stamps a reconcile wrote onto the corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StampsReconcileReport {
    /// The embedding model the stamps name.
    pub embed_model: String,
    /// The vector width the live embedder answered with.
    pub vector_dim: u32,
    /// The chunking-behaviour version this build plans with.
    pub chunk_version: u32,
    /// The normalization version this build hashes with.
    pub normalize_version: u32,
}

impl From<IndexStamps> for StampsReconcileReport {
    fn from(stamps: IndexStamps) -> Self {
        StampsReconcileReport {
            embed_model: stamps.embed_model,
            vector_dim: stamps.vector_dim,
            chunk_version: stamps.chunk_version,
            normalize_version: stamps.normalize_version,
        }
    }
}

/// Probe the live embedder for its vector width and write this build's
/// index stamps onto the book corpus. Returns the stamps written.
pub async fn reconcile(cfg: &Config) -> Result<StampsReconcileReport> {
    let embed_cfg = crate::profile::effective_embed_config(cfg)?;
    let embedder = embedder(cfg, &embed_cfg)?;
    // Probe the embedder once for its current vector dimension. The
    // probe is the only network call this command makes; the corpus
    // write happens locally.
    let probe = embedder
        .embed_batch(&[bookrack_embed::DIMENSION_PROBE_TEXT.to_string()])
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
    Ok(StampsReconcileReport::from(stamps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_corpus::{
        CHUNK_VERSION_KEY, EMBED_MODEL_KEY, NORMALIZE_VERSION_KEY, VECTOR_DIM_KEY,
    };
    use bookrack_test_support::EmbedStub;

    /// The report is what landed on the corpus: each field reads back
    /// from the stamp it names, and the width is the one the embedder
    /// answered the probe with.
    #[tokio::test]
    async fn reconcile_reports_the_stamps_it_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(dir.path().to_path_buf(), EmbedStub::url().to_string());
        Corpus::open(&cfg.corpus_db()).expect("seed corpus");

        let report = reconcile(&cfg).await.expect("reconcile");

        assert_eq!(report.vector_dim as usize, EmbedStub::DIMENSION);
        let corpus = Corpus::open(&cfg.corpus_db()).expect("reopen corpus");
        let stamp = |key: &str| corpus.meta_get(key).expect("read stamp");
        assert_eq!(stamp(EMBED_MODEL_KEY), Some(report.embed_model.clone()));
        assert_eq!(stamp(VECTOR_DIM_KEY), Some(report.vector_dim.to_string()));
        assert_eq!(
            stamp(CHUNK_VERSION_KEY),
            Some(report.chunk_version.to_string())
        );
        assert_eq!(
            stamp(NORMALIZE_VERSION_KEY),
            Some(report.normalize_version.to_string())
        );
    }
}
