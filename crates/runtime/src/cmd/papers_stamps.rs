// SPDX-License-Identifier: Apache-2.0

//! `bookrack papers stamps reconcile` — write the live embedder's model
//! name and vector dimension into `papers_corpus.db`'s `index_meta`
//! table. Peer of [`crate::cmd::stamps`] for the paper pipeline.

use bookrack_config::Config;
use bookrack_corpus::Corpus;
use eyre::{Context, ContextCompat, Result};

use crate::cmd::stamps::StampsReconcileReport;
use crate::embed_helpers::embedder;

/// Probe the live embedder for its vector width and write the paper
/// pipeline's index stamps onto `papers_corpus.db`. Returns the stamps
/// written.
pub async fn reconcile(cfg: &Config) -> Result<StampsReconcileReport> {
    let embed_cfg = crate::profile::effective_embed_config(cfg)?;
    let embedder = embedder(cfg, &embed_cfg)?;
    let probe = embedder
        .embed_batch(&[bookrack_embed::DIMENSION_PROBE_TEXT.to_string()])
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
    Ok(StampsReconcileReport::from(stamps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_corpus::{
        CHUNK_VERSION_KEY, EMBED_MODEL_KEY, NORMALIZE_VERSION_KEY, VECTOR_DIM_KEY,
    };
    use bookrack_test_support::EmbedStub;

    /// The paper-side report reads back from `papers_corpus.db`, not
    /// from the book corpus.
    #[tokio::test]
    async fn reconcile_reports_the_stamps_it_wrote_on_the_paper_corpus() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(dir.path().to_path_buf(), EmbedStub::url().to_string());
        Corpus::open(&cfg.papers_corpus_db()).expect("seed paper corpus");

        let report = reconcile(&cfg).await.expect("reconcile");

        assert_eq!(report.vector_dim as usize, EmbedStub::DIMENSION);
        let corpus = Corpus::open(&cfg.papers_corpus_db()).expect("reopen paper corpus");
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
