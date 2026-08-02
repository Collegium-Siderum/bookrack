// SPDX-License-Identifier: Apache-2.0

//! Paper-side vector-store writes against `lancedb_papers`: ANN
//! rebuild, brute-force drop, re-embed under the active embedder, and
//! reset+rechunk. Peer of [`crate::cmd::vectors`] for the paper
//! pipeline; status reads live at
//! `bookrack rpc call library.vectors_status`.

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::Config;
use bookrack_corpus::{Corpus, EMBED_MODEL_KEY, VECTOR_DIM_KEY};
use bookrack_vectors::ChunkStore;
use eyre::{Context, Result};

use crate::cmd::input_error::CmdInputError;
use crate::cmd::vectors::parse_ann_kind;
use crate::embed_helpers::embedder;
use crate::pipeline_run_helpers::{close_pass_run, open_pass_run};

/// Render `bookrack papers vectors rebuild` — build or rebuild the ANN
/// index over `lancedb_papers` from CLI flags, falling back to the
/// persisted meta or the C1 recommended default for any flag not
/// supplied.
#[allow(clippy::too_many_arguments)]
pub async fn rebuild(
    cfg: &Config,
    kind_str: Option<&str>,
    num_partitions: Option<u32>,
    num_sub_vectors: Option<u32>,
    num_bits: Option<u32>,
    nprobes: Option<u32>,
    refine_factor: Option<u32>,
) -> Result<()> {
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
    println!(
        "rebuilt: kind={} np={}",
        base.kind.as_str(),
        base.num_partitions
    );
    Ok(())
}

/// Render `bookrack papers vectors drop` — drop any ANN index over
/// `lancedb_papers` and stamp the meta as `kind = brute-force`. Search
/// falls back to a full scan.
pub async fn drop(cfg: &Config) -> Result<()> {
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
    println!("dropped: kind=brute-force");
    Ok(())
}

/// Render `bookrack papers vectors reset` — drop the papers chunks
/// table, clear the papers_corpus stamps, and re-embed every paper's
/// abstract leaf with the env-configured embedding model. The old
/// vectors are unrecoverable.
pub async fn reset<F>(cfg: &Config, yes: bool, resume: bool, ask: F) -> Result<()>
where
    F: FnOnce(&str) -> Result<bool>,
{
    let lancedb_dir = cfg.papers_lancedb_dir();
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let embed_cfg = crate::profile::effective_embed_config(cfg)?;
    let embedder_client = embedder(cfg, &embed_cfg)?;

    let embedded_intakes = catalog
        .intakes_with_status(IntakeStatus::Embedded)
        .context("count embedded paper intakes")?;
    let extracted_intakes = catalog
        .intakes_with_status(IntakeStatus::Extracted)
        .context("count extracted paper intakes")?;
    let current_model = corpus
        .meta_get(EMBED_MODEL_KEY)
        .context("read embed_model stamp")?;
    let current_dim = corpus
        .meta_get(VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?;
    let store_dim = ChunkStore::try_open(&lancedb_dir)
        .await
        .context("probe papers chunk store")?
        .map(|s| s.dimension());

    if resume {
        if extracted_intakes.is_empty() {
            println!(
                "nothing to resume: no paper intakes are in the Extracted state.\n\
                 If you meant to start a fresh reset, drop --resume."
            );
            return Ok(());
        }
        println!(
            "resume reset: {} paper intake(s) in Extracted will be re-embedded with model '{}'.",
            extracted_intakes.len(),
            embed_cfg.model
        );
    } else {
        println!("papers vectors reset plan:");
        match (current_model.as_deref(), current_dim.as_deref()) {
            (Some(m), Some(d)) => println!("  current library: model='{m}', dim={d}"),
            _ => println!("  current library: no stamps recorded"),
        }
        match store_dim {
            Some(d) => println!("  chunks table:    dim={d}"),
            None => println!("  chunks table:    absent"),
        }
        println!(
            "  target model:    '{}' (probed dim deferred to first embed)",
            embed_cfg.model
        );
        println!(
            "  affected:        {} Embedded paper intake(s) -> will be re-embedded",
            embedded_intakes.len()
        );
        if !extracted_intakes.is_empty() {
            println!(
                "  also pending:    {} Extracted paper intake(s) already waiting",
                extracted_intakes.len()
            );
        }
        println!(
            "This drops the papers chunks table, clears the papers_corpus index\n\
             stamps, and reembeds every paper's abstract leaf from the corpus\n\
             node tree. The old vectors are unrecoverable. Restart the daemon\n\
             after this completes so it picks up the new model."
        );
        let prompt = "Type RESET (exact, uppercase) to continue: ";
        if !yes && !ask(prompt)? {
            println!("aborted; no changes written");
            return Ok(());
        }
    }

    // Registered past the confirmation prompt: a declined reset is not
    // a pass and leaves no row behind.
    let pipeline_run_id = open_pass_run(&catalog, "papers_reset", cfg.data_dir().to_str());
    let outcome = bookrack_glean::reset::reset_and_rechunk(
        &catalog,
        &mut corpus,
        &lancedb_dir,
        &embedder_client,
        &embed_cfg,
        resume,
    )
    .await;
    close_pass_run(&catalog, pipeline_run_id.as_deref(), outcome.is_ok());

    let report = match outcome {
        Ok(report) => report,
        Err(e) => {
            // A mid-build failure leaves finished intakes at Embedded
            // and the failing one at Extracted, so a resume continues
            // where this run stopped.
            eprintln!(
                "reset did not finish; rerun with `bookrack papers vectors reset --resume` \
                 once the cause is addressed"
            );
            return Err(e).context("papers reset_and_rechunk");
        }
    };

    println!(
        "reset complete: {} paper intake(s) re-embedded, {} chunk row(s) written",
        report.intakes_reembedded, report.chunks_written,
    );
    if !report.skipped_empty.is_empty() {
        println!("skipped (no abstract leaf): {:?}", report.skipped_empty);
    }
    println!("restart the daemon so the new model takes effect.");
    Ok(())
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
    let pipeline_run_id = open_pass_run(&catalog, "papers_reembed", cfg.data_dir().to_str());
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
    close_pass_run(&catalog, pipeline_run_id.as_deref(), result.is_ok());
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

        reset(&cfg, true, false, |_| Ok(true))
            .await
            .expect("reset succeeds on an empty library");

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

    /// Declining the confirmation prompt is not a pass.
    #[tokio::test]
    async fn a_declined_reset_registers_nothing() {
        let (_tmp, cfg) = temp_cfg();

        reset(&cfg, false, false, |_| Ok(false))
            .await
            .expect("declining is not an error");

        assert!(
            runs(&cfg).is_empty(),
            "no run row for a pass that never ran"
        );
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
