// SPDX-License-Identifier: Apache-2.0

//! REPL-side vector-store writes: ANN rebuild, brute-force drop, and
//! re-embed against the active model. Status reads live at
//! `bookrack exec library.vectors_status`.

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::{Corpus, EMBED_MODEL_KEY, VECTOR_DIM_KEY};
use bookrack_vectors::ChunkStore;
use eyre::{Context, Result};

use crate::embed_helpers::embedder;

/// Render `bookrack vectors rebuild` — build or rebuild the ANN index
/// from CLI flags, falling back to the persisted meta or the C1
/// recommended default for any flag not supplied.
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
    let lancedb_dir = cfg.lancedb_dir();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or_else(|| {
            eyre::eyre!("library has no ingested chunks yet; ingest a book before rebuild")
        })?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open vector store")?;
    // Pick the baseline: explicit kind > existing meta > default IvfFlat.
    let mut base = if let Some(s) = kind_str {
        let kind: bookrack_vectors::AnnKind =
            s.parse().with_context(|| format!("parse --kind {s:?}"))?;
        bookrack_vectors::AnnConfig::default_for(kind)
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
    println!(
        "rebuilt: kind={} np={}",
        base.kind.as_str(),
        base.num_partitions
    );
    Ok(())
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
    let embed_cfg = EmbedConfig::from_env();
    let embedder_client = embedder(cfg, &embed_cfg)?;
    bookrack_ingest::reembed::reembed_all(
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
    .context("reembed_all")
}

/// Render `bookrack vectors drop` — drop any ANN index and stamp the
/// meta as `kind = brute-force`. Search falls back to a full scan.
pub async fn drop(cfg: &Config) -> Result<()> {
    let lancedb_dir = cfg.lancedb_dir();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or_else(|| eyre::eyre!("library has no ingested chunks yet; nothing to drop"))?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open vector store")?;
    store
        .drop_ann_index(&lancedb_dir, bookrack_ingest::now_rfc3339())
        .await
        .context("drop ann index")?;
    println!("dropped: kind=brute-force");
    Ok(())
}

/// Render `bookrack vectors reset` — drop the chunks table, clear the
/// corpus stamps, and re-embed every book with the env-configured
/// embedding model. The old vectors are unrecoverable. Use after
/// switching `BOOKRACK_EMBED_MODEL`; for a non-destructive trial of a
/// new model, use `libraries fork` instead.
pub async fn reset<F>(cfg: &Config, yes: bool, resume: bool, ask: F) -> Result<()>
where
    F: FnOnce(&str) -> Result<bool>,
{
    let lancedb_dir = cfg.lancedb_dir();
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let embed_cfg = EmbedConfig::from_env();
    let embedder_client = embedder(cfg, &embed_cfg)?;

    let embedded_intakes = catalog
        .intakes_with_status(IntakeStatus::Embedded)
        .context("count embedded intakes")?;
    let extracted_intakes = catalog
        .intakes_with_status(IntakeStatus::Extracted)
        .context("count extracted intakes")?;
    let current_model = corpus
        .meta_get(EMBED_MODEL_KEY)
        .context("read embed_model stamp")?;
    let current_dim = corpus
        .meta_get(VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?;
    let store_dim = ChunkStore::try_open(&lancedb_dir)
        .await
        .context("probe chunk store")?
        .map(|s| s.dimension());

    if resume {
        if extracted_intakes.is_empty() {
            println!(
                "nothing to resume: no intakes are in the Extracted state.\n\
                 If you meant to start a fresh reset, drop --resume."
            );
            return Ok(());
        }
        println!(
            "resume reset: {} intake(s) in Extracted will be re-embedded with model '{}'.",
            extracted_intakes.len(),
            embed_cfg.model
        );
    } else {
        println!("vectors reset plan:");
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
            "  affected:        {} Embedded intake(s) -> will be re-embedded",
            embedded_intakes.len()
        );
        if !extracted_intakes.is_empty() {
            println!(
                "  also pending:    {} Extracted intake(s) already waiting",
                extracted_intakes.len()
            );
        }
        println!(
            "This drops the chunks table, clears the corpus index stamps, and\n\
             reembeds every book from the corpus node tree. The old vectors are\n\
             unrecoverable. Restart the daemon after this completes so it picks\n\
             up the new model."
        );
        let prompt = "Type RESET (exact, uppercase) to continue: ";
        if !yes && !ask(prompt)? {
            println!("aborted; no changes written");
            return Ok(());
        }
    }

    let report = bookrack_ingest::reset::reset_and_rechunk(
        &catalog,
        &corpus,
        &lancedb_dir,
        &embedder_client,
        &embed_cfg,
        resume,
    )
    .await
    .context("reset_and_rechunk")?;

    println!(
        "reset complete: {} intake(s) re-embedded, {} chunk row(s) written",
        report.intakes_reembedded, report.chunks_written,
    );
    if !report.skipped_empty.is_empty() {
        println!("skipped (no prose leaves): {:?}", report.skipped_empty);
    }
    if let Some(failed) = report.failed_intake {
        println!(
            "intake {failed} failed; rerun with `bookrack vectors reset --resume` once the cause is addressed",
        );
    } else {
        println!("restart the daemon so the new model takes effect.");
    }
    Ok(())
}
