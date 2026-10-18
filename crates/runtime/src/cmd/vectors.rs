// SPDX-License-Identifier: Apache-2.0

//! REPL-side vector-store writes: ANN rebuild, brute-force drop, and
//! re-embed against the active model. Status reads live at
//! `bookrack rpc call library.vectors_status`.

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::Config;
use bookrack_corpus::Corpus;
use bookrack_vectors::{AnnConfig, AnnKind, ChunkStore};
use eyre::{Context, Result};
use serde::Serialize;

use crate::cmd::ResetInterrupted;
use crate::cmd::input_error::CmdInputError;
use crate::embed_helpers::{embedder, refuse_dimension_mismatch};
use crate::pipeline_run_helpers::{close_run, open_run};

/// The ANN configuration a rebuild built the index with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnnRebuildReport {
    /// The index kind, spelled as `--kind` takes it.
    pub kind: &'static str,
    /// IVF partition count.
    pub num_partitions: u32,
    /// PQ sub-vector count, for the PQ kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_sub_vectors: Option<u32>,
    /// PQ code width, for the PQ kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_bits: Option<u32>,
    /// Query-time partition fan-out.
    pub nprobes: u32,
    /// Query-time refinement multiplier, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refine_factor: Option<u32>,
}

impl From<&AnnConfig> for AnnRebuildReport {
    fn from(cfg: &AnnConfig) -> Self {
        AnnRebuildReport {
            kind: cfg.kind.as_str(),
            num_partitions: cfg.num_partitions,
            num_sub_vectors: cfg.num_sub_vectors,
            num_bits: cfg.num_bits,
            nprobes: cfg.nprobes,
            refine_factor: cfg.refine_factor,
        }
    }
}

/// The index kind a drop left the store at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnnDropReport {
    /// Always `brute-force`: search falls back to a full scan.
    pub kind: &'static str,
}

impl AnnDropReport {
    pub(crate) fn brute_force() -> Self {
        AnnDropReport {
            kind: AnnKind::BruteForce.as_str(),
        }
    }
}

/// What one reset pass re-embedded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct VectorsResetReport {
    /// Whether the pass continued an interrupted reset instead of
    /// starting a fresh one.
    pub resumed: bool,
    /// Intakes this pass re-embedded.
    pub intakes_reembedded: usize,
    /// Chunk rows this pass wrote.
    pub chunks_written: usize,
    /// Intakes this pass found nothing to chunk in; they stay at
    /// `Extracted`.
    pub skipped_empty: Vec<i64>,
}

/// Parse a `--kind` value, reporting an unsupported one against the
/// accepted set.
///
/// The set is read off [`AnnKind::ALL`] rather than transcribed, so a
/// new kind reaches this message without anyone remembering to add it.
/// The parse's own error is dropped: its wording describes a
/// `vectors_meta.json` this caller never named.
pub(crate) fn parse_ann_kind(s: &str) -> Result<AnnKind> {
    s.parse().map_err(|_| {
        CmdInputError::BadArgument {
            arg: "kind",
            value: s.to_string(),
            expected: AnnKind::ALL
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        }
        .into()
    })
}

/// Run `bookrack vectors rebuild` — build or rebuild the ANN index
/// from CLI flags, falling back to the persisted meta or the C1
/// recommended default for any flag not supplied. Returns the
/// configuration the index was built with.
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
    let lancedb_dir = cfg.lancedb_dir();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or(CmdInputError::NotIngested {
            what: "ingested chunks",
            hint: "Ingest a book before rebuilding the index.",
        })?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open vector store")?;
    // Pick the baseline: explicit kind > existing meta > default IvfFlat.
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
        .context("build ann index")?;
    Ok(AnnRebuildReport::from(&base))
}

/// Compute a reembed plan without writing anything. Returns the
/// per-intake plan rows a subsequent
/// [`execute_reembed_from_plan`] call will pin to.
pub async fn plan_reembed(
    cfg: &Config,
    book: Option<i64>,
    stale_only: bool,
) -> Result<Vec<bookrack_ingest::reembed::ReembedPlan>> {
    let lancedb_dir = cfg.lancedb_dir();
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    bookrack_ingest::reembed::plan_reembed(&catalog, &lancedb_dir, book, None, stale_only)
        .await
        .context("plan reembed")
}

/// Execute a reembed against the exact pinned set computed by an
/// earlier [`plan_reembed`] call. Strict: every id in `pinned_ids`
/// must still resolve to an Embedded catalog row, else the call
/// aborts without writing.
pub async fn execute_reembed_from_plan(
    cfg: &Config,
    pinned_ids: Vec<i64>,
) -> Result<bookrack_ingest::reembed::ReembedReport> {
    let lancedb_dir = cfg.lancedb_dir();
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let embed_cfg = crate::profile::effective_embed_config(cfg)?;
    let embedder_client = embedder(cfg, &embed_cfg)?;
    let run = open_run(
        &catalog,
        &cfg.catalog_db(),
        "reembed",
        cfg.data_dir().to_str(),
    );
    let result = bookrack_ingest::reembed::reembed_all(
        &catalog,
        &corpus,
        &lancedb_dir,
        &embed_cfg,
        &embedder_client,
        None,
        Some(&pinned_ids),
        false,
    )
    .await
    .context("reembed_all");
    close_run(&catalog, run, result.is_ok());
    result
}

/// Run `bookrack vectors drop` — drop any ANN index and stamp the
/// meta as `kind = brute-force`. Search falls back to a full scan.
pub async fn drop(cfg: &Config) -> Result<AnnDropReport> {
    let lancedb_dir = cfg.lancedb_dir();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or(CmdInputError::NotIngested {
            what: "ingested chunks",
            hint: "There is no index to drop until a book has been ingested.",
        })?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open vector store")?;
    store
        .drop_ann_index(&lancedb_dir, bookrack_ingest::now_rfc3339())
        .await
        .context("drop ann index")?;
    Ok(AnnDropReport::brute_force())
}

/// Run `bookrack vectors reset` — drop the chunks table, clear the
/// corpus stamps, and re-embed every book with the index profile's
/// embedding model. The old vectors are unrecoverable. Use after
/// pointing the library at a profile that declares a different model;
/// for a non-destructive trial, use `libraries fork` instead. The
/// caller confirms; this function does not ask.
///
/// When an index profile is in effect, its `embed.dim` is checked
/// against the backend before anything changes, and a dimension the
/// backend does not produce is refused as caller input.
///
/// With `resume`, continue a reset that stopped partway: the
/// destructive steps are skipped and the intakes still at `Extracted`
/// are re-embedded. A resume with none at `Extracted` does nothing,
/// registers no run, and reports zero.
///
/// A failure of the reset pass itself carries [`ResetInterrupted`]; a
/// failure while setting it up does not.
pub async fn reset(cfg: &Config, resume: bool) -> Result<VectorsResetReport> {
    let lancedb_dir = cfg.lancedb_dir();
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let (embed_cfg, declaration) = crate::profile::effective_embed_declaration(cfg)?;
    let embedder_client = embedder(cfg, &embed_cfg)?;

    if resume
        && catalog
            .intakes_with_status(IntakeStatus::Extracted)
            .context("count extracted intakes")?
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
        &cfg.catalog_db(),
        "reset",
        cfg.data_dir().to_str(),
    );
    let outcome = bookrack_ingest::reset::reset_and_rechunk(
        &catalog,
        &corpus,
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

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::Catalog;
    use bookrack_core::PartitionIdx;
    use bookrack_corpus::VECTOR_DIM_KEY;
    use bookrack_vectors::ChunkRow;

    const DIM: usize = 4;

    fn temp_cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(
            dir.path().to_path_buf(),
            "http://localhost:11434".to_string(),
        );
        // `drop` names this module's command function, so the seed
        // handles are released by falling out of the statement.
        Catalog::open(&cfg.catalog_db()).expect("seed catalog");
        Corpus::open(&cfg.corpus_db()).expect("seed corpus");
        (dir, cfg)
    }

    fn runs(cfg: &Config) -> Vec<(String, Option<String>)> {
        let catalog = Catalog::open_read_only(&cfg.catalog_db()).expect("open read-only");
        catalog
            .list_pipeline_runs(None, None)
            .expect("list pipeline_runs")
            .into_iter()
            .map(|run| (run.command, run.status))
            .collect()
    }

    /// Stamp the corpus with the store width and write `n` chunk rows,
    /// enough for a one-partition IVF index to train on.
    async fn seed_chunks(cfg: &Config, n: i64) {
        Corpus::open(&cfg.corpus_db())
            .expect("open corpus")
            .meta_set(VECTOR_DIM_KEY, &DIM.to_string())
            .expect("stamp vector_dim");
        let store = ChunkStore::open(&cfg.lancedb_dir(), DIM)
            .await
            .expect("open store");
        let rows: Vec<ChunkRow> = (1..=n)
            .map(|offset| {
                let node = PartitionIdx::new(1).node_id(offset).expect("in range");
                let v = offset as f32 / n as f32;
                ChunkRow {
                    vector: vec![v, 1.0 - v, 0.5, 0.25],
                    text: format!("chunk {offset}"),
                    start_node_id: node,
                    start_char_offset: 0,
                    end_node_id: node,
                    end_char_offset: 10,
                    norm_chunk_sha256: format!("sha-{offset}"),
                }
            })
            .collect();
        store.append(&rows).await.expect("append rows");
    }

    /// The ANN configuration as the store's own sidecar records it.
    async fn stored_ann_cfg(cfg: &Config) -> AnnConfig {
        ChunkStore::open(&cfg.lancedb_dir(), DIM)
            .await
            .expect("open store")
            .current_ann_cfg(&cfg.lancedb_dir())
            .expect("read ann config")
            .expect("an index was recorded")
    }

    /// The report is the configuration the index was built with: the
    /// flags given, the kind's defaults for the rest, and the same
    /// values the store's sidecar holds afterwards.
    #[tokio::test]
    async fn rebuild_reports_the_configuration_it_built() {
        let (_tmp, cfg) = temp_cfg();
        seed_chunks(&cfg, 300).await;

        let report = rebuild(&cfg, Some("ivf-flat"), Some(1), None, None, Some(3), None)
            .await
            .expect("rebuild");

        assert_eq!(
            report,
            AnnRebuildReport {
                kind: "ivf-flat",
                num_partitions: 1,
                num_sub_vectors: None,
                num_bits: None,
                nprobes: 3,
                refine_factor: None,
            }
        );
        assert_eq!(
            report,
            AnnRebuildReport::from(&stored_ann_cfg(&cfg).await),
            "the report must describe what the store recorded"
        );
    }

    #[tokio::test]
    async fn drop_reports_the_brute_force_kind_it_left() {
        let (_tmp, cfg) = temp_cfg();
        seed_chunks(&cfg, 4).await;

        let report = drop(&cfg).await.expect("drop");

        assert_eq!(report.kind, "brute-force");
        assert_eq!(stored_ann_cfg(&cfg).await.kind.as_str(), report.kind);
    }

    /// A whole-library pass registers one run row and closes it with a
    /// terminal status, so `bookrack runs list` shows that the pass
    /// happened. An empty library still counts as a pass: the run is
    /// about the invocation, not about how many items it touched.
    #[tokio::test]
    async fn reset_registers_and_closes_one_run() {
        let (_tmp, cfg) = temp_cfg();

        let report = reset(&cfg, false)
            .await
            .expect("reset succeeds on an empty library");

        assert_eq!(report, VectorsResetReport::default());
        assert_eq!(
            runs(&cfg),
            vec![("reset".to_string(), Some("ok".to_string()))]
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

    /// The reembed pass registers on its execute leg. The plan leg is
    /// a dry run and must stay out of the registry.
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
            vec![("reembed".to_string(), Some("ok".to_string()))]
        );
    }
}
