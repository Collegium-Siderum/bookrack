// SPDX-License-Identifier: Apache-2.0

//! `papers.corpus_rebuild` — regenerate the paper corpus tree from the
//! opaque envelopes stored in `papers_dir`, optionally re-embedding the
//! abstract chunks. Drives the daemon's pinned two-phase protocol via
//! [`plan_rebuild`] + [`execute_rebuild_from_plan`]; mirrors
//! [`crate::cmd::corpus`] for the paper pipeline.

// Parallel implementation: runtime-cmd-corpus.

use bookrack_catalog::Catalog;
use bookrack_config::Config;
use bookrack_corpus::Corpus;
use eyre::{Context, Result};

use crate::embed_helpers::embedder;

/// Compute a papers rebuild plan without writing anything. Returns
/// the classification buckets a subsequent
/// [`execute_rebuild_from_plan`] call will pin to.
///
/// The catalog is read through its read-only door and the corpus is
/// not opened. With no catalog on disk the plan is empty, unless
/// `paper` names an intake, which is then reported as unknown.
pub fn plan_rebuild(
    cfg: &Config,
    paper: Option<i64>,
    stale_only: bool,
) -> Result<bookrack_ops::pipeline::paper::rebuild::RebuildReport> {
    let Some(catalog) = Catalog::open(&cfg.papers_catalog_db()).context("open papers catalog")?
    else {
        return match paper {
            Some(id) => Err(bookrack_ops::pipeline::paper::GleanError::UnknownIntake(id))
                .context("plan papers rebuild"),
            None => Ok(bookrack_ops::pipeline::paper::rebuild::RebuildReport::default()),
        };
    };
    let params = bookrack_ops::pipeline::paper::rebuild::RebuildParams {
        only: paper,
        stale_only,
        dry_run: true,
        ..Default::default()
    };
    bookrack_ops::pipeline::paper::rebuild::classify_rebuild_targets(&catalog, &params)
        .context("plan papers rebuild")
}

/// Aggregate outcome of [`execute_rebuild_from_plan`]: the rebuild
/// classification plus any L0 stamp / L2 reembed sidecar work the
/// execute leg performed. Mirrors
/// [`crate::cmd::corpus::ExecuteRebuildOutcome`].
#[derive(Debug, Clone, Default)]
pub struct ExecutePapersRebuildOutcome {
    pub report: bookrack_ops::pipeline::paper::rebuild::RebuildReport,
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
/// else the call aborts without writing. An empty `pinned_ids` returns
/// the default outcome before any store is opened.
pub async fn execute_rebuild_from_plan(
    cfg: &Config,
    daemon: &bookrack_config::DaemonConfig,
    pinned_ids: Vec<i64>,
    include_vectors: bool,
) -> Result<ExecutePapersRebuildOutcome> {
    if pinned_ids.is_empty() {
        return Ok(ExecutePapersRebuildOutcome::default());
    }
    let mut corpus =
        Corpus::open_for_write(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let catalog = Catalog::open_for_write(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;

    let report = bookrack_ops::pipeline::paper::rebuild::rebuild_from_intakes(
        &mut corpus,
        &catalog,
        &bookrack_ops::pipeline::paper::rebuild::RebuildParams {
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
        let embed_cfg = crate::profile::effective_embed_config(cfg, daemon)?;
        let embedder_client = embedder(&embed_cfg)?;
        let reembed = bookrack_ops::pipeline::paper::reembed::reembed_all(
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
        let embed_cfg = crate::profile::effective_embed_config(cfg, daemon)?;
        let stamped = bookrack_ops::pipeline::paper::rebuild::stamp_index_from_existing_chunks(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::test_fixtures::{store_files, temp_cfg};
    use bookrack_catalog::fixtures;

    #[test]
    fn plan_on_an_empty_root_creates_no_store() {
        let (tmp, cfg) = temp_cfg();

        let report = plan_rebuild(&cfg, None, false).expect("plan on an empty root");

        assert!(report.rebuilt.is_empty(), "{report:?}");
        assert!(
            store_files(tmp.path()).is_empty(),
            "a rebuild plan created {:?}",
            store_files(tmp.path()),
        );
    }

    #[test]
    fn plan_on_a_behind_catalog_neither_migrates_it_nor_creates_the_corpus() {
        let (tmp, cfg) = temp_cfg();
        let behind = fixtures::CURRENT_VERSION - 1;
        fixtures::create_at_version(&cfg.papers_catalog_db(), behind);

        // Whether a behind catalog yields a plan is not under test.
        let _ = plan_rebuild(&cfg, None, false);

        assert_eq!(
            fixtures::user_version(&cfg.papers_catalog_db()),
            behind,
            "a rebuild plan migrated the catalog",
        );
        assert!(
            !cfg.papers_corpus_db().exists(),
            "a rebuild plan created {:?}",
            store_files(tmp.path()),
        );
    }

    #[test]
    fn plan_naming_an_unknown_paper_on_an_empty_root_reports_it_and_creates_no_store() {
        let (tmp, cfg) = temp_cfg();

        let err = plan_rebuild(&cfg, Some(9_999_999), false)
            .expect_err("an id no catalog holds cannot be planned");

        assert!(
            err.chain().any(|cause| matches!(
                cause.downcast_ref::<bookrack_ops::pipeline::paper::GleanError>(),
                Some(bookrack_ops::pipeline::paper::GleanError::UnknownIntake(
                    9_999_999
                ))
            )),
            "{err:?}",
        );
        assert!(
            store_files(tmp.path()).is_empty(),
            "a rebuild plan created {:?}",
            store_files(tmp.path()),
        );
    }

    #[tokio::test]
    async fn executing_an_empty_plan_on_an_empty_root_creates_no_store() {
        let (tmp, cfg) = temp_cfg();

        let outcome = execute_rebuild_from_plan(
            &cfg,
            &crate::cmd::test_fixtures::test_daemon(),
            Vec::new(),
            false,
        )
        .await
        .expect("an empty plan executes");

        assert!(outcome.report.rebuilt.is_empty(), "{outcome:?}");
        assert!(
            outcome.stamped_from_existing_chunks.is_none(),
            "{outcome:?}"
        );
        assert!(outcome.reembed.is_none(), "{outcome:?}");
        assert!(
            store_files(tmp.path()).is_empty(),
            "executing an empty plan created {:?}",
            store_files(tmp.path()),
        );
    }
}
