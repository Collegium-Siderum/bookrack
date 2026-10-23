// SPDX-License-Identifier: Apache-2.0

//! Paper-side vector-store writes against `lancedb_papers`: ANN
//! rebuild, brute-force drop, re-embed under the active embedder, and
//! reset+rechunk. Peer of [`crate::cmd::vectors`] for the paper
//! pipeline; status reads live at
//! `bookrack rpc call library.vectors_status`.

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::Config;
use bookrack_corpus::Corpus;
use bookrack_vectors::ChunkStore;
use eyre::{Context, Result};

use crate::cmd::ResetInterrupted;
use crate::cmd::input_error::CmdInputError;
use crate::cmd::vectors::{AnnDropReport, AnnRebuildReport, VectorsResetReport, parse_ann_kind};
use crate::embed_helpers::{embedder, refuse_dimension_mismatch};
use crate::pipeline_run_helpers::{close_run, open_run};

/// Run `bookrack papers vectors rebuild` — build or rebuild the ANN
/// index over `lancedb_papers` from CLI flags, falling back to the
/// persisted meta or the C1 recommended default for any flag not
/// supplied. Returns the configuration the index was built with.
#[allow(clippy::too_many_arguments)]
pub async fn rebuild(
    cfg: &Config,
    kind_str: Option<&str>,
    num_partitions: Option<u32>,
    num_sub_vectors: Option<u32>,
    num_bits: Option<u32>,
    nprobes: Option<u32>,
    refine_factor: Option<u32>,
) -> Result<AnnRebuildReport> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or(CmdInputError::NotIngested {
            what: "ingested paper chunks",
            hint: "Glean a paper before rebuilding the index.",
        })?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open papers vector store")?;
    let mut base = if let Some(s) = kind_str {
        bookrack_vectors::AnnConfig::default_for(parse_ann_kind(s)?)
    } else if let Some(c) = store
        .current_ann_cfg(&lancedb_dir)
        .context("read ann config")?
    {
        c
    } else {
        bookrack_vectors::AnnConfig::default_for(bookrack_vectors::AnnKind::IvfFlat)
    };
    if let Some(v) = num_partitions {
        base.num_partitions = v;
    }
    if let Some(v) = num_sub_vectors {
        base.num_sub_vectors = Some(v);
    }
    if let Some(v) = num_bits {
        base.num_bits = Some(v);
    }
    if let Some(v) = nprobes {
        base.nprobes = v;
    }
    if let Some(v) = refine_factor {
        base.refine_factor = Some(v);
    }
    store
        .build_ann_index(&base, &lancedb_dir, bookrack_ingest::now_rfc3339())
        .await
        .context("build papers ann index")?;
    Ok(AnnRebuildReport::from(&base))
}

/// Run `bookrack papers vectors drop` — drop any ANN index over
/// `lancedb_papers` and stamp the meta as `kind = brute-force`. Search
/// falls back to a full scan.
pub async fn drop(cfg: &Config) -> Result<AnnDropReport> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or(CmdInputError::NotIngested {
            what: "ingested paper chunks",
            hint: "There is no index to drop until a paper has been gleaned.",
        })?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open papers vector store")?;
    store
        .drop_ann_index(&lancedb_dir, bookrack_ingest::now_rfc3339())
        .await
        .context("drop papers ann index")?;
    Ok(AnnDropReport::brute_force())
}

/// Run `bookrack papers vectors reset` — drop the papers chunks
/// table, clear the papers_corpus stamps, and re-embed every paper's
/// abstract leaf with the index profile's embedding model. The old
/// vectors are unrecoverable. The caller confirms; this function does
/// not ask.
///
/// When an index profile is in effect, its `embed.dim` is checked
/// against the backend before anything changes, and a dimension the
/// backend does not produce is refused as caller input.
///
/// With `resume`, continue a reset that stopped partway: the
/// destructive steps are skipped and the paper intakes still at
/// `Extracted` are re-embedded. A resume with none at `Extracted` does
/// nothing, registers no run, and reports zero.
///
/// A failure of the reset pass itself carries [`ResetInterrupted`]; a
/// failure while setting it up does not.
pub async fn reset(cfg: &Config, resume: bool) -> Result<VectorsResetReport> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let (embed_cfg, declaration) = crate::profile::effective_embed_declaration(cfg)?;
    let embedder_client = embedder(cfg, &embed_cfg)?;

    if resume
        && catalog
            .intakes_with_status(IntakeStatus::Extracted)
            .context("count extracted paper intakes")?
            .is_empty()
    {
        return Ok(VectorsResetReport {
            resumed: true,
            ..VectorsResetReport::default()
        });
    }

    refuse_dimension_mismatch(&embedder_client, &embed_cfg.model, declaration.as_ref()).await?;

    let run = open_run(
        &catalog,
        &cfg.papers_catalog_db(),
        "papers_reset",
        cfg.data_dir().to_str(),
    );
    let outcome = bookrack_glean::reset::reset_and_rechunk(
        &catalog,
        &mut corpus,
        &lancedb_dir,
        &embedder_client,
        &embed_cfg,
        resume,
    )
    .await;
    close_run(&catalog, run, outcome.is_ok());
    // A failure here leaves finished intakes at Embedded and the
    // failing one at Extracted, so a resume continues where it stopped.
    let report = outcome.wrap_err(ResetInterrupted)?;
    Ok(VectorsResetReport {
        resumed: resume,
        intakes_reembedded: report.intakes_reembedded,
        chunks_written: report.chunks_written,
        skipped_empty: report.skipped_empty,
    })
}

/// Compute a papers reembed plan without writing anything. Returns
/// the per-intake plan rows a subsequent
/// [`execute_reembed_from_plan`] call will pin to.
pub async fn plan_reembed(
    cfg: &Config,
    paper: Option<i64>,
    stale_only: bool,
) -> Result<Vec<bookrack_glean::reembed::ReembedPlan>> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    bookrack_glean::reembed::plan_reembed(&catalog, &lancedb_dir, paper, None, stale_only)
        .await
        .context("plan papers reembed")
}

/// Execute a papers reembed against the exact pinned set computed by
/// an earlier [`plan_reembed`] call. Strict: every id in
/// `pinned_ids` must still resolve to an Embedded catalog row, else
/// the call aborts without writing.
pub async fn execute_reembed_from_plan(
    cfg: &Config,
    pinned_ids: Vec<i64>,
) -> Result<bookrack_glean::reembed::ReembedReport> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let embed_cfg = crate::profile::effective_embed_config(cfg)?;
    let embedder_client = embedder(cfg, &embed_cfg)?;
    let run = open_run(
        &catalog,
        &cfg.papers_catalog_db(),
        "papers_reembed",
        cfg.data_dir().to_str(),
    );
    let result = bookrack_glean::reembed::reembed_all(
        &catalog,
        &mut corpus,
        &lancedb_dir,
        &embed_cfg,
        &embedder_client,
        None,
        Some(&pinned_ids),
        false,
    )
    .await
    .context("papers reembed_all");
    close_run(&catalog, run, result.is_ok());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(
            dir.path().to_path_buf(),
            "http://localhost:11434".to_string(),
        );
        // `drop` names this module's command function, so the seed
        // handles are released by falling out of the statement.
        Catalog::open(&cfg.papers_catalog_db()).expect("seed paper catalog");
        Corpus::open(&cfg.papers_corpus_db()).expect("seed paper corpus");
        (dir, cfg)
    }

    fn runs(cfg: &Config) -> Vec<(String, Option<String>)> {
        let catalog = Catalog::open_read_only(&cfg.papers_catalog_db()).expect("open read-only");
        catalog
            .list_pipeline_runs(None, None)
            .expect("list pipeline_runs")
            .into_iter()
            .map(|run| (run.command, run.status))
            .collect()
    }

    /// The paper-side pass registers on the paper catalog, under its
    /// own command name: a merged `runs list` has to tell a paper
    /// reset from a book one.
    #[tokio::test]
    async fn reset_registers_and_closes_one_run_on_the_paper_catalog() {
        let (_tmp, cfg) = temp_cfg();

        let report = reset(&cfg, false)
            .await
            .expect("reset succeeds on an empty library");

        assert_eq!(report, VectorsResetReport::default());
        assert_eq!(
            runs(&cfg),
            vec![("papers_reset".to_string(), Some("ok".to_string()))]
        );
        let books = Catalog::open_read_only(&cfg.catalog_db());
        assert!(
            books.is_err()
                || books
                    .expect("open")
                    .list_pipeline_runs(None, None)
                    .expect("list")
                    .is_empty(),
            "the book catalog carries no paper-side run row",
        );
    }

    /// A resume with nothing at `Extracted` is not a pass: it reports
    /// that it resumed and did nothing, and leaves no run row.
    #[tokio::test]
    async fn a_resume_with_nothing_to_resume_reports_zero_and_registers_nothing() {
        let (_tmp, cfg) = temp_cfg();

        let report = reset(&cfg, true).await.expect("resume");

        assert_eq!(
            report,
            VectorsResetReport {
                resumed: true,
                ..VectorsResetReport::default()
            }
        );
        assert!(runs(&cfg).is_empty(), "{:?}", runs(&cfg));
    }

    /// The reembed pass registers on its execute leg; the plan leg is
    /// a dry run and stays out of the registry.
    #[tokio::test]
    async fn reembed_registers_on_the_execute_leg_only() {
        let (_tmp, cfg) = temp_cfg();

        let plan = plan_reembed(&cfg, None, false).await.expect("plan");
        assert!(plan.is_empty(), "an empty library plans no work");
        assert!(runs(&cfg).is_empty(), "the plan leg registers nothing");

        execute_reembed_from_plan(&cfg, Vec::new())
            .await
            .expect("execute");

        assert_eq!(
            runs(&cfg),
            vec![("papers_reembed".to_string(), Some("ok".to_string()))]
        );
    }
}
