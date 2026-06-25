// SPDX-License-Identifier: Apache-2.0

//! `papers.corpus_rebuild` — regenerate the paper corpus tree from the
//! opaque envelopes stored in `papers_dir`, optionally re-embedding the
//! abstract chunks. Drives the daemon's pinned two-phase protocol via
//! [`plan_rebuild`] + [`execute_rebuild_from_plan`]; mirrors
//! [`crate::cmd::corpus`] for the paper pipeline.

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;

use crate::embed_helpers::embedder;

/// Compute a papers rebuild plan without writing anything. Returns
/// the classification buckets a subsequent
/// [`execute_rebuild_from_plan`] call will pin to.
pub fn plan_rebuild(
    cfg: &Config,
    paper: Option<i64>,
    stale_only: bool,
) -> Result<bookrack_glean::rebuild::RebuildReport> {
    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let params = bookrack_glean::rebuild::RebuildParams {
        only: paper,
        stale_only,
        dry_run: true,
        ..Default::default()
    };
    bookrack_glean::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &params)
        .context("plan papers rebuild")
}

/// Aggregate outcome of [`execute_rebuild_from_plan`]: the rebuild
/// classification plus any L0 stamp / L2 reembed sidecar work the
/// execute leg performed. Mirrors
/// [`crate::cmd::corpus::ExecuteRebuildOutcome`].
#[derive(Debug, Clone, Default)]
pub struct ExecutePapersRebuildOutcome {
    pub report: bookrack_glean::rebuild::RebuildReport,
    /// `Some(true)` if `stamp_index_from_existing_chunks` wrote
    /// stamps; `Some(false)` if the chunks table was empty so no
    /// stamping happened; `None` when `include_vectors` took
    /// precedence and stamping was skipped intentionally.
    pub stamped_from_existing_chunks: Option<bool>,
    /// Set when `include_vectors = true`: number of intakes
    /// reembedded and total chunks written.
    pub reembed: Option<ReembedSummary>,
}

/// Counts surfaced when an execute included a follow-up reembed.
#[derive(Debug, Clone, Default)]
pub struct ReembedSummary {
    pub intakes: usize,
    pub chunks_written: usize,
}

/// Execute a papers rebuild against the exact pinned set computed by
/// an earlier [`plan_rebuild`] call. Strict: every id in
/// `pinned_ids` must still resolve to a rebuildable catalog row,
/// else the call aborts without writing.
pub async fn execute_rebuild_from_plan(
    cfg: &Config,
    pinned_ids: Vec<i64>,
    include_vectors: bool,
) -> Result<ExecutePapersRebuildOutcome> {
    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;

    let report = bookrack_glean::rebuild::rebuild_from_intakes(
        &mut corpus,
        &catalog,
        &bookrack_glean::rebuild::RebuildParams {
            only_ids: Some(pinned_ids),
            dry_run: false,
            ..Default::default()
        },
    )
    .context("papers rebuild")?;

    let mut outcome = ExecutePapersRebuildOutcome {
        report,
        ..Default::default()
    };
    if outcome.report.rebuilt.is_empty() {
        return Ok(outcome);
    }

    if include_vectors {
        let lancedb_dir = cfg.papers_lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let embedder_client = embedder(cfg, &embed_cfg)?;
        let reembed = bookrack_glean::reembed::reembed_all(
            &catalog,
            &mut corpus,
            &lancedb_dir,
            &embed_cfg,
            &embedder_client,
            None,
            Some(&outcome.report.rebuilt),
            false,
        )
        .await
        .context("papers reembed after rebuild")?;
        let chunks_written: usize = reembed.intakes.iter().map(|o| o.chunks_written).sum();
        outcome.reembed = Some(ReembedSummary {
            intakes: reembed.intakes.len(),
            chunks_written,
        });
    } else {
        let lancedb_dir = cfg.papers_lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let stamped = bookrack_glean::rebuild::stamp_index_from_existing_chunks(
            &corpus,
            &lancedb_dir,
            &embed_cfg.model,
        )
        .await
        .context("stamp papers index_meta after rebuild")?;
        outcome.stamped_from_existing_chunks = Some(stamped);
    }
    Ok(outcome)
}
