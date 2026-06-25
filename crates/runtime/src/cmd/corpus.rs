// SPDX-License-Identifier: Apache-2.0

//! `corpus.rebuild` — regenerate the corpus tree from the opaque
//! envelopes, optionally re-embedding. Drives the daemon's pinned
//! two-phase protocol: [`plan_rebuild`] computes the dry-run report
//! and [`execute_rebuild_from_plan`] acts on the exact pinned target
//! set, so drift between the dry-run and the execute leg cannot leak
//! into the destructive operation.

use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;
use eyre::{Context, Result};

use crate::embed_helpers::embedder;

/// Compute a rebuild plan without writing anything. Returns the
/// classification buckets a subsequent [`execute_rebuild_from_plan`]
/// call will pin to.
pub fn plan_rebuild(
    cfg: &Config,
    book: Option<i64>,
    stale_only: bool,
) -> Result<bookrack_ingest::rebuild::RebuildReport> {
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let params = bookrack_ingest::rebuild::RebuildParams {
        only: book,
        stale_only,
        dry_run: true,
        ..Default::default()
    };
    bookrack_ingest::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &params)
        .context("plan rebuild")
}

/// Aggregate outcome of [`execute_rebuild_from_plan`]: the rebuild
/// classification plus any L0 stamp / L2 reembed sidecar work the
/// execute leg performed.
#[derive(Debug, Clone, Default)]
pub struct ExecuteRebuildOutcome {
    pub report: bookrack_ingest::rebuild::RebuildReport,
    /// `Some(true)` if `stamp_index_from_existing_chunks` wrote
    /// stamps; `Some(false)` if the chunks table was empty so no
    /// stamping happened; `None` when the `include_vectors` path
    /// took precedence and stamping was skipped intentionally.
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

/// Execute a rebuild against the exact pinned set computed by an
/// earlier [`plan_rebuild`] call. Strict: every id in `pinned_ids`
/// must still resolve to a rebuildable catalog row, else the call
/// aborts without writing.
pub async fn execute_rebuild_from_plan(
    cfg: &Config,
    pinned_ids: Vec<i64>,
    include_vectors: bool,
) -> Result<ExecuteRebuildOutcome> {
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;

    let report = bookrack_ingest::rebuild::rebuild_from_intakes(
        &mut corpus,
        &catalog,
        &bookrack_ingest::rebuild::RebuildParams {
            only_ids: Some(pinned_ids),
            dry_run: false,
            ..Default::default()
        },
    )
    .context("rebuild")?;

    let mut outcome = ExecuteRebuildOutcome {
        report,
        ..Default::default()
    };
    if outcome.report.rebuilt.is_empty() {
        return Ok(outcome);
    }

    if include_vectors {
        let lancedb_dir = cfg.lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let embedder_client = embedder(cfg, &embed_cfg)?;
        let reembed = bookrack_ingest::reembed::reembed_all(
            &catalog,
            &corpus,
            &lancedb_dir,
            &embed_cfg,
            &embedder_client,
            None,
            Some(&outcome.report.rebuilt),
            false,
        )
        .await
        .context("reembed after rebuild")?;
        let chunks_written: usize = reembed
            .intakes
            .iter()
            .map(|o| o.embed_run.chunks_written)
            .sum();
        outcome.reembed = Some(ReembedSummary {
            intakes: reembed.intakes.len(),
            chunks_written,
        });
    } else {
        let lancedb_dir = cfg.lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let stamped = bookrack_ingest::rebuild::stamp_index_from_existing_chunks(
            &corpus,
            &lancedb_dir,
            &embed_cfg.model,
        )
        .await
        .context("stamp index_meta after rebuild")?;
        outcome.stamped_from_existing_chunks = Some(stamped);
    }
    Ok(outcome)
}
