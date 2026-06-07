// SPDX-License-Identifier: Apache-2.0

//! `bookrack vectors` — vector-store status, ANN rebuild, brute-force
//! drop, and re-embed against the active model.

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;
use bookrack_ops::dto::vectors_status::VectorsStatus;
use bookrack_vectors::ChunkStore;

use crate::embed_helpers::embedder;
use crate::ops_helpers::catalog_only_ops;
use crate::util::confirm;

/// Render `bookrack vectors status` — a read-only summary of the
/// table, the LanceDB index it carries, and the persisted ANN config.
pub async fn status(cfg: &Config) -> Result<()> {
    let ops = catalog_only_ops(cfg);
    let status = bookrack_ops::reads::vectors::status(&ops)
        .await
        .context("read vectors status")?;
    print_status(&status);
    Ok(())
}

/// Write the status DTO to stdout. Pure over its input so the formatter
/// can be exercised against a fixed snapshot without touching disk.
fn print_status(status: &VectorsStatus) {
    match status.row_count {
        None => {
            println!("table:           (empty -- no chunks ingested yet)");
            println!("ann index:       (none)");
            println!("ann config:      (no meta)");
            println!("churn:           n/a");
            return;
        }
        Some(rows) => println!("table:           {rows} rows"),
    }
    if status.indices.is_empty() {
        println!("ann index:       (none -- brute-force)");
    } else {
        for idx in &status.indices {
            println!("ann index:       {}", idx.name);
            if let Some(s) = &idx.stats {
                println!("  type:          {}", s.index_type);
                println!("  num_indexed:   {}", s.num_indexed_rows);
                println!("  num_unindexed: {}", s.num_unindexed_rows);
                if let Some(ni) = s.num_indices {
                    println!("  num_indices:   {ni}");
                }
                if let Some(loss) = s.loss {
                    println!("  loss:          {loss}");
                } else {
                    println!("  loss:          n/a");
                }
            }
        }
    }
    match &status.ann_config {
        None => println!("ann config:      (no meta)"),
        Some(c) => println!(
            "ann config:      {} / np={} / nprobes={} / refine={}",
            c.kind,
            c.num_partitions,
            c.nprobes,
            c.refine_factor
                .map(|r| r.to_string())
                .unwrap_or_else(|| "n/a".to_string())
        ),
    }
    match &status.meta {
        None => println!("churn:           n/a"),
        Some(m) => println!(
            "churn:           {} since last rebuild",
            m.churn_since_rebuild
        ),
    }
    if let Some(drift) = &status.meta_drift {
        println!(
            "meta drift:      expected index {:?}, found {:?}; \
             run bookrack vectors rebuild",
            drift.expected_index, drift.found_indices
        );
    }
}

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
            anyhow::anyhow!("library has no ingested chunks yet; ingest a book before rebuild")
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

/// Render `bookrack vectors reembed` — read each book's chunks back
/// from LanceDB, drop the vectors, and run the active embedder over
/// them. Use after switching `embed_model` / `embed_dim`.
pub async fn reembed(
    cfg: &Config,
    book: Option<i64>,
    stale_only: bool,
    dry_run: bool,
    yes: bool,
    profile_name: Option<&str>,
) -> Result<()> {
    let lancedb_dir = cfg.lancedb_dir();
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let plans = bookrack_ingest::reembed::plan_reembed(&catalog, &lancedb_dir, book, stale_only)
        .await
        .context("plan reembed")?;
    if plans.is_empty() {
        if stale_only {
            println!("no stale partitions; nothing to reembed");
        } else {
            println!("no embedded intakes carry chunks; nothing to reembed");
        }
        return Ok(());
    }
    let total_chunks: usize = plans.iter().map(|p| p.chunk_count).sum();
    let total_chars: usize = plans.iter().map(|p| p.total_chars).sum();
    println!("reembed plan ({} intakes):", plans.len());
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
    let prompt = "About to delete-and-rewrite the chunk rows above.\n\
                  Existing vectors will be overwritten by fresh embeddings\n\
                  from the currently configured model. This is irreversible.\n\
                  Type 'yes' to continue: ";
    if !yes && !confirm(prompt)? {
        println!("aborted; no changes written");
        return Ok(());
    }

    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let _ = profile_name;
    let embed_cfg = EmbedConfig::from_env();
    let embedder_client = embedder(cfg, &embed_cfg)?;
    let report = bookrack_ingest::reembed::reembed_all(
        &catalog,
        &corpus,
        &lancedb_dir,
        &embed_cfg,
        &embedder_client,
        book,
        stale_only,
    )
    .await
    .context("reembed_all")?;
    let _ = &mut corpus;

    let total_written: usize = report
        .intakes
        .iter()
        .map(|o| o.embed_run.chunks_written)
        .sum();
    let total_batches: usize = report.intakes.iter().map(|o| o.embed_run.batches).sum();
    let total_shrinks: usize = report
        .intakes
        .iter()
        .map(|o| o.embed_run.shrink_events)
        .sum();
    println!(
        "reembedded: {} intakes / {} chunks / {} batches / {} shrinks",
        report.intakes.len(),
        total_written,
        total_batches,
        total_shrinks
    );
    if !report.skipped_empty.is_empty() {
        println!("skipped (no chunks): {:?}", report.skipped_empty);
    }
    Ok(())
}

/// Render `bookrack vectors drop` — drop any ANN index and stamp the
/// meta as `kind = brute-force`. Search falls back to a full scan.
pub async fn drop(cfg: &Config) -> Result<()> {
    let lancedb_dir = cfg.lancedb_dir();
    let corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let dim = corpus
        .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
        .context("read vector_dim stamp")?
        .ok_or_else(|| anyhow::anyhow!("library has no ingested chunks yet; nothing to drop"))?
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
