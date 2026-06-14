// SPDX-License-Identifier: Apache-2.0

//! `bookrack papers corpus rebuild` — regenerate the paper corpus tree
//! from the opaque envelopes stored in `papers_dir`, optionally
//! re-embedding the abstract chunks.

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;

use crate::embed_helpers::embedder;

#[allow(clippy::too_many_arguments)]
pub async fn rebuild<F>(
    cfg: &Config,
    include_vectors: bool,
    paper: Option<i64>,
    stale_only: bool,
    dry_run: bool,
    yes: bool,
    ask: F,
) -> Result<()>
where
    F: FnOnce(&str) -> Result<bool>,
{
    let mut corpus = Corpus::open(&cfg.papers_corpus_db()).context("open papers corpus")?;
    let catalog = Catalog::open_with_backup(&cfg.papers_catalog_db(), &cfg.backup_dir())
        .context("open papers catalog")?;

    let plan_params = bookrack_glean::rebuild::RebuildParams {
        only: paper,
        stale_only,
        dry_run: true,
    };
    let plan_report =
        bookrack_glean::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &plan_params)
            .context("plan papers rebuild")?;
    println!(
        "papers rebuild plan: {} rebuildable, {} missing_envelope, {} mismatched, {} failed",
        plan_report.rebuilt.len(),
        plan_report.missing_envelope.len(),
        plan_report.mismatched.len(),
        plan_report.failed.len()
    );
    if !plan_report.missing_envelope.is_empty() {
        println!("  missing_envelope: {:?}", plan_report.missing_envelope);
    }
    if !plan_report.mismatched.is_empty() {
        println!("  mismatched:       {:?}", plan_report.mismatched);
    }
    if !plan_report.failed.is_empty() {
        for (id, err) in &plan_report.failed {
            println!("  failed:           intake {id}: {err}");
        }
    }
    if dry_run {
        return Ok(());
    }
    if plan_report.rebuilt.is_empty() {
        println!("no rebuildable paper intakes; aborting");
        return Ok(());
    }

    let prompt = if include_vectors {
        "About to overwrite papers_corpus.db node rows for the intakes\n\
         above, then re-embed each paper's abstract into lancedb_papers.\n\
         This is irreversible (the existing corpus tree is replaced).\n\
         Type 'yes' to continue: "
    } else {
        "About to overwrite papers_corpus.db node rows for the intakes\n\
         above. lancedb_papers will retain its current chunks; the\n\
         index_meta build stamps are re-stamped from the existing chunks\n\
         so search can continue to run. Re-embed with\n\
         `bookrack papers vectors reembed` if you bumped the chunking\n\
         or normalization algorithm. This is irreversible (the existing\n\
         corpus tree is replaced).\n\
         Type 'yes' to continue: "
    };
    if !yes && !ask(prompt)? {
        println!("aborted; no changes written");
        return Ok(());
    }

    let run_params = bookrack_glean::rebuild::RebuildParams {
        only: paper,
        stale_only,
        dry_run: false,
    };
    let report = bookrack_glean::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &run_params)
        .context("papers rebuild")?;
    println!(
        "rebuilt: {} intakes ({} missing_envelope, {} mismatched, {} failed)",
        report.rebuilt.len(),
        report.missing_envelope.len(),
        report.mismatched.len(),
        report.failed.len()
    );

    if !include_vectors && !report.rebuilt.is_empty() {
        let lancedb_dir = cfg.papers_lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let stamped = bookrack_glean::rebuild::stamp_index_from_existing_chunks(
            &corpus,
            &lancedb_dir,
            &embed_cfg.model,
        )
        .await
        .context("stamp papers index_meta after rebuild")?;
        if !stamped {
            println!(
                "no chunks on disk; papers index_meta stamps were not written. \
                 Run `bookrack papers vectors reembed` after embedding to enable search."
            );
        }
    }

    if include_vectors && !report.rebuilt.is_empty() {
        let lancedb_dir = cfg.papers_lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let embedder_client = embedder(cfg, &embed_cfg)?;
        let reembed = bookrack_glean::reembed::reembed_all(
            &catalog,
            &mut corpus,
            &lancedb_dir,
            &embed_cfg,
            &embedder_client,
            paper,
            stale_only,
        )
        .await
        .context("papers reembed after rebuild")?;
        let total_written: usize = reembed.intakes.iter().map(|o| o.chunks_written).sum();
        println!(
            "reembedded: {} intakes / {} chunks",
            reembed.intakes.len(),
            total_written
        );
    }
    Ok(())
}
