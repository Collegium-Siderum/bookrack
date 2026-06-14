// SPDX-License-Identifier: Apache-2.0

//! Paper-side vector-store writes against `lancedb_papers`: ANN
//! rebuild, brute-force drop, re-embed under the active embedder, and
//! reset+rechunk. Peer of [`crate::cmd::vectors`] for the paper
//! pipeline; status reads live at
//! `bookrack exec library.vectors_status`.

use anyhow::{Context, Result};
use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::{Corpus, EMBED_MODEL_KEY, VECTOR_DIM_KEY};
use bookrack_vectors::ChunkStore;

use crate::embed_helpers::embedder;

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
        .ok_or_else(|| {
            anyhow::anyhow!(
                "papers library has no ingested chunks yet; glean a paper before rebuild"
            )
        })?
        .parse::<usize>()
        .context("parse vector_dim stamp")?;
    let store = ChunkStore::open(&lancedb_dir, dim)
        .await
        .context("open papers vector store")?;
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
        .context("build papers ann index")?;
    println!(
        "rebuilt: kind={} np={}",
        base.kind.as_str(),
        base.num_partitions
    );
    Ok(())
}

/// Render `bookrack papers vectors reembed` — read each paper's chunks
/// back from `lancedb_papers`, drop the vectors, and run the active
/// embedder over them. Use after switching `embed_model` / `embed_dim`.
pub async fn reembed<F>(
    cfg: &Config,
    paper: Option<i64>,
    stale_only: bool,
    dry_run: bool,
    yes: bool,
    ask: F,
) -> Result<()>
where
    F: FnOnce(&str) -> Result<bool>,
{
    let lancedb_dir = cfg.papers_lancedb_dir();
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;
    let plans = bookrack_glean::reembed::plan_reembed(&catalog, &lancedb_dir, paper, stale_only)
        .await
        .context("plan papers reembed")?;
    if plans.is_empty() {
        if stale_only {
            println!("no stale paper partitions; nothing to reembed");
        } else {
            println!("no embedded paper intakes carry chunks; nothing to reembed");
        }
        return Ok(());
    }
    let total_chunks: usize = plans.iter().map(|p| p.chunk_count).sum();
    let total_chars: usize = plans.iter().map(|p| p.total_chars).sum();
    println!("papers reembed plan ({} intakes):", plans.len());
    for plan in &plans {
        println!(
            "  intake {:>4}: {:>5} chunks, {:>9} chars",
            plan.intake_id, plan.chunk_count, plan.total_chars
        );
    }
    println!(
        "totals:        {:>5} chunks, {:>9} chars",
        total_chunks, total_chars
    );
    if dry_run {
        return Ok(());
    }
    let prompt = "About to delete-and-rewrite the paper chunk rows above.\n\
                  Existing vectors will be overwritten by fresh embeddings\n\
                  from the currently configured model. This is irreversible.\n\
                  Type 'yes' to continue: ";
    if !yes && !ask(prompt)? {
        println!("aborted; no changes written");
        return Ok(());
    }

    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let embed_cfg = EmbedConfig::from_env();
    let embedder_client = embedder(cfg, &embed_cfg)?;
    let report = bookrack_glean::reembed::reembed_all(
        &catalog,
        &mut corpus,
        &lancedb_dir,
        &embed_cfg,
        &embedder_client,
        paper,
        stale_only,
    )
    .await
    .context("papers reembed_all")?;

    let total_written: usize = report.intakes.iter().map(|o| o.chunks_written).sum();
    println!(
        "reembedded: {} intakes / {} chunks",
        report.intakes.len(),
        total_written
    );
    if !report.skipped_empty.is_empty() {
        println!("skipped (no chunks): {:?}", report.skipped_empty);
    }
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
        .ok_or_else(|| {
            anyhow::anyhow!("papers library has no ingested chunks yet; nothing to drop")
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
    let embed_cfg = EmbedConfig::from_env();
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

    let report = bookrack_glean::reset::reset_and_rechunk(
        &catalog,
        &mut corpus,
        &lancedb_dir,
        &embedder_client,
        &embed_cfg,
        resume,
    )
    .await
    .context("papers reset_and_rechunk")?;

    println!(
        "reset complete: {} paper intake(s) re-embedded, {} chunk row(s) written",
        report.intakes_reembedded, report.chunks_written,
    );
    if !report.skipped_empty.is_empty() {
        println!("skipped (no abstract leaf): {:?}", report.skipped_empty);
    }
    if let Some(failed) = report.failed_intake {
        println!(
            "intake {failed} failed; rerun with `bookrack papers vectors reset --resume` once the cause is addressed",
        );
    } else {
        println!("restart the daemon so the new model takes effect.");
    }
    Ok(())
}
