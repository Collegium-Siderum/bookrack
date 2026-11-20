// SPDX-License-Identifier: Apache-2.0

//! Paper-side vector-store writes against `lancedb_papers`: ANN
//! rebuild, brute-force drop, re-embed under the active embedder, and
//! reset+rechunk. Peer of [`crate::cmd::vectors`] for the paper
//! pipeline; status reads live at
//! `bookrack rpc call library.vectors_status`.

// Parallel implementation: runtime-cmd-vectors.

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::Config;
use bookrack_corpus::Corpus;
use bookrack_vectors::{ChunkReader, ChunkStore};
use eyre::{Context, Result};

use crate::cmd::ResetInterrupted;
use crate::cmd::input_error::CmdInputError;
use crate::cmd::state_error::CmdStateError;
use crate::cmd::vectors::{AnnDropReport, AnnRebuildReport, VectorsResetReport, parse_ann_kind};
use crate::embed_helpers::{embedder, refuse_dimension_mismatch};
use crate::pipeline_run_helpers::{close_run, open_run};

/// Open the chunks table an ANN command works on. The vector dimension
/// is read from the corpus stamp through a read-only door and the table
/// is probed before the write door is taken, so a library with nothing
/// to index is refused with nothing created: `not_ingested` when the
/// corpus or its stamp is absent, [`CmdStateError::VectorTableMissing`]
/// when the stamp is there and the table is not.
async fn open_ann_store_for_write(cfg: &Config, not_ingested: CmdInputError) -> Result<ChunkStore> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let stamp = match Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")? {
        Some(corpus) => corpus
            .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
            .context("read vector_dim stamp")?,
        None => None,
    };
    let dim = stamp
        .ok_or(not_ingested)?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    if ChunkReader::open(&lancedb_dir)
        .await
        .context("open papers vector store")?
        .is_none()
    {
        return Err(CmdStateError::VectorTableMissing {
            dim,
            reset_command: "bookrack papers vectors reset",
        }
        .into());
    }
    ChunkStore::open_for_write(&lancedb_dir, dim)
        .await
        .context("open papers vector store")
}

/// Whether any intake is still at `Extracted`, read without creating or
/// migrating the catalog. A root with no catalog has none.
fn has_extracted_intakes(cfg: &Config) -> Result<bool> {
    let Some(catalog) = Catalog::open(&cfg.papers_catalog_db()).context("open papers catalog")?
    else {
        return Ok(false);
    };
    Ok(!catalog
        .intakes_with_status(bookrack_core::ItemKind::Paper, IntakeStatus::Extracted)
        .context("count extracted paper intakes")?
        .is_empty())
}

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
    let store = open_ann_store_for_write(
        cfg,
        CmdInputError::NotIngested {
            what: "ingested paper chunks",
            hint: "Glean a paper before rebuilding the index.",
        },
    )
    .await?;
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
        .build_ann_index(
            &base,
            &lancedb_dir,
            bookrack_ops::pipeline::book::now_rfc3339(),
        )
        .await
        .context("build papers ann index")?;
    Ok(AnnRebuildReport::from(&base))
}

/// Run `bookrack papers vectors drop` — drop any ANN index over
/// `lancedb_papers` and stamp the meta as `kind = brute-force`. Search
/// falls back to a full scan.
pub async fn drop(cfg: &Config) -> Result<AnnDropReport> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let store = open_ann_store_for_write(
        cfg,
        CmdInputError::NotIngested {
            what: "ingested paper chunks",
            hint: "There is no index to drop until a paper has been gleaned.",
        },
    )
    .await?;
    store
        .drop_ann_index(&lancedb_dir, bookrack_ops::pipeline::book::now_rfc3339())
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
pub async fn reset(
    cfg: &Config,
    daemon: &bookrack_config::DaemonConfig,
    resume: bool,
) -> Result<VectorsResetReport> {
    if resume && !has_extracted_intakes(cfg)? {
        return Ok(VectorsResetReport {
            resumed: true,
            ..VectorsResetReport::default()
        });
    }

    let (embed_cfg, declaration) = crate::profile::effective_embed_declaration(cfg, daemon)?;
    let embedder_client = embedder(&embed_cfg)?;
    refuse_dimension_mismatch(&embedder_client, &embed_cfg.model, declaration.as_ref()).await?;

    let lancedb_dir = cfg.papers_lancedb_dir();
    let catalog = Catalog::open_for_write(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let mut corpus =
        Corpus::open_for_write(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let run = open_run(
        &catalog,
        &cfg.papers_catalog_db(),
        "papers_reset",
        cfg.data_dir().to_str(),
    );
    let outcome = bookrack_ops::pipeline::paper::reset::reset_and_rechunk(
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
///
/// The catalog is read through its read-only door. With no catalog on
/// disk the plan is empty, unless `paper` names an intake, which is
/// then reported as unknown.
pub async fn plan_reembed(
    cfg: &Config,
    paper: Option<i64>,
    stale_only: bool,
) -> Result<Vec<bookrack_ops::pipeline::paper::reembed::ReembedPlan>> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let Some(catalog) = Catalog::open(&cfg.papers_catalog_db()).context("open papers catalog")?
    else {
        return match paper {
            Some(id) => Err(bookrack_ops::pipeline::paper::GleanError::UnknownIntake(id))
                .context("plan papers reembed"),
            None => Ok(Vec::new()),
        };
    };
    bookrack_ops::pipeline::paper::reembed::plan_reembed(
        &catalog,
        &lancedb_dir,
        paper,
        None,
        stale_only,
    )
    .await
    .context("plan papers reembed")
}

/// Execute a papers reembed against the exact pinned set computed by
/// an earlier [`plan_reembed`] call. Strict: every id in
/// `pinned_ids` must still resolve to an Embedded catalog row, else
/// the call aborts without writing.
pub async fn execute_reembed_from_plan(
    cfg: &Config,
    daemon: &bookrack_config::DaemonConfig,
    pinned_ids: Vec<i64>,
) -> Result<bookrack_ops::pipeline::paper::reembed::ReembedReport> {
    let lancedb_dir = cfg.papers_lancedb_dir();
    let catalog = Catalog::open_for_write(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let mut corpus =
        Corpus::open_for_write(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let embed_cfg = crate::profile::effective_embed_config(cfg, daemon)?;
    let embedder_client = embedder(&embed_cfg)?;
    let run = open_run(
        &catalog,
        &cfg.papers_catalog_db(),
        "papers_reembed",
        cfg.data_dir().to_str(),
    );
    let result = bookrack_ops::pipeline::paper::reembed::reembed_all(
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
        let cfg = Config::new(dir.path().to_path_buf());
        // `drop` names this module's command function, so the seed
        // handles are released by falling out of the statement.
        Catalog::open_for_write_unbacked(&cfg.papers_catalog_db()).expect("seed paper catalog");
        Corpus::open_for_write(&cfg.papers_corpus_db()).expect("seed paper corpus");
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

        let report = reset(&cfg, &crate::cmd::test_fixtures::test_daemon(), false)
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

        let report = reset(&cfg, &crate::cmd::test_fixtures::test_daemon(), true)
            .await
            .expect("resume");

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

        execute_reembed_from_plan(&cfg, &crate::cmd::test_fixtures::test_daemon(), Vec::new())
            .await
            .expect("execute");

        assert_eq!(
            runs(&cfg),
            vec![("papers_reembed".to_string(), Some("ok".to_string()))]
        );
    }

    use crate::cmd::test_fixtures::{store_files, temp_cfg as empty_cfg};

    fn is_not_ingested(err: &eyre::Report) -> bool {
        err.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<CmdInputError>(),
                Some(CmdInputError::NotIngested { .. })
            )
        })
    }

    #[tokio::test]
    async fn plan_reembed_on_an_empty_root_creates_no_store() {
        let (tmp, cfg) = empty_cfg();

        let plan = plan_reembed(&cfg, None, false)
            .await
            .expect("plan on an empty root");

        assert!(plan.is_empty(), "an empty root plans no work");
        assert!(
            store_files(tmp.path()).is_empty(),
            "a reembed plan created {:?}",
            store_files(tmp.path()),
        );
    }

    #[tokio::test]
    async fn plan_reembed_naming_an_unknown_paper_on_an_empty_root_reports_it_and_creates_no_store()
    {
        let (tmp, cfg) = empty_cfg();

        let err = plan_reembed(&cfg, Some(9_999_999), false)
            .await
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
            "a reembed plan created {:?}",
            store_files(tmp.path()),
        );
    }

    #[tokio::test]
    async fn ann_rebuild_and_drop_on_an_empty_root_report_not_ingested_and_create_no_store() {
        let (tmp, cfg) = empty_cfg();

        let err = rebuild(&cfg, None, None, None, None, None, None)
            .await
            .expect_err("nothing to index on an empty root");
        assert!(is_not_ingested(&err), "rebuild: {err:?}");
        assert!(
            store_files(tmp.path()).is_empty(),
            "an index rebuild created {:?}",
            store_files(tmp.path()),
        );

        let err = drop(&cfg)
            .await
            .expect_err("no index to drop on an empty root");
        assert!(is_not_ingested(&err), "drop: {err:?}");
        assert!(
            store_files(tmp.path()).is_empty(),
            "an index drop created {:?}",
            store_files(tmp.path()),
        );
    }

    #[tokio::test]
    async fn ann_rebuild_and_drop_refuse_a_stamped_corpus_whose_vector_table_is_missing() {
        let (_tmp, cfg) = empty_cfg();
        Corpus::open_for_write(&cfg.papers_corpus_db())
            .expect("create corpus")
            .meta_set(bookrack_corpus::VECTOR_DIM_KEY, "4")
            .expect("stamp vector_dim");

        let is_table_missing = |err: &eyre::Report| {
            err.chain().any(|cause| {
                matches!(
                    cause.downcast_ref::<CmdStateError>(),
                    Some(CmdStateError::VectorTableMissing { dim: 4, .. })
                )
            })
        };

        let err = rebuild(&cfg, None, None, None, None, None, None)
            .await
            .expect_err("a stamp with no table behind it is not a state to index");
        assert!(
            !cfg.papers_lancedb_dir().exists(),
            "an index rebuild created the vector table it found missing",
        );
        assert!(is_table_missing(&err), "rebuild: {err:?}");

        let err = drop(&cfg)
            .await
            .expect_err("a stamp with no table behind it has no index to drop");
        assert!(
            !cfg.papers_lancedb_dir().exists(),
            "an index drop created the vector table it found missing",
        );
        assert!(is_table_missing(&err), "drop: {err:?}");
    }

    #[tokio::test]
    async fn a_resume_on_an_empty_root_reports_zero_and_creates_no_store() {
        let (tmp, cfg) = empty_cfg();

        let report = reset(&cfg, &crate::cmd::test_fixtures::test_daemon(), true)
            .await
            .expect("resume on an empty root");

        assert_eq!(
            report,
            VectorsResetReport {
                resumed: true,
                ..VectorsResetReport::default()
            }
        );
        assert!(
            store_files(tmp.path()).is_empty(),
            "a resume with nothing to resume created {:?}",
            store_files(tmp.path()),
        );
    }

    #[tokio::test]
    async fn a_reset_refused_on_dimension_creates_no_store() {
        let stub = bookrack_test_support::EmbedStub::start();
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(tmp.path().to_path_buf());
        bookrack_config::set_manifest_index_profile(
            tmp.path(),
            Some(bookrack_index_profile::PROFILE_QWEN3_06B_DEFAULT),
            bookrack_config::ManifestIdentitySeed {
                name: "lib",
                kind: &bookrack_config::LibraryKind::Test,
                description: None,
                registered_uuid: None,
            },
        )
        .expect("write a manifest declaring a built-in profile");

        let err = reset(
            &cfg,
            &crate::cmd::test_fixtures::daemon_at(stub.url()),
            false,
        )
        .await
        .expect_err("the stub's width is not the profile's");

        assert!(
            err.chain().any(|cause| matches!(
                cause.downcast_ref::<CmdInputError>(),
                Some(CmdInputError::Refused { .. })
            )),
            "{err:?}",
        );
        assert!(
            store_files(tmp.path()).is_empty(),
            "a refused reset created {:?}",
            store_files(tmp.path()),
        );
    }
}
