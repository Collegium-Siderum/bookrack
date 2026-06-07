// SPDX-License-Identifier: Apache-2.0

//! `bookrack corpus rebuild` — regenerate the corpus tree from the
//! opaque envelopes, optionally re-embedding.

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;

use crate::embed_helpers::embedder;
use crate::util::confirm;

#[allow(clippy::too_many_arguments)]
pub async fn rebuild(
    cfg: &Config,
    include_vectors: bool,
    book: Option<i64>,
    stale_only: bool,
    dry_run: bool,
    yes: bool,
    profile_name: Option<&str>,
) -> Result<()> {
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;

    let plan_params = bookrack_ingest::rebuild::RebuildParams {
        only: book,
        stale_only,
        dry_run: true,
        ..Default::default()
    };
    let plan_report =
        bookrack_ingest::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &plan_params)
            .context("plan rebuild")?;
    println!(
        "rebuild plan: {} rebuildable, {} missing_envelope, {} mismatched, {} failed",
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
        println!("no rebuildable intakes; aborting");
        return Ok(());
    }

    let prompt = if include_vectors {
        "About to overwrite corpus.db node rows for the intakes above,\n\
         then re-embed each book's chunks into LanceDB. This is\n\
         irreversible (the existing corpus tree is replaced).\n\
         Type 'yes' to continue: "
    } else {
        "About to overwrite corpus.db node rows for the intakes above.\n\
         LanceDB will retain its current chunks; the index_meta build\n\
         stamps are re-stamped from the existing chunks so search can\n\
         continue to run. Re-embed with `bookrack vectors reembed`\n\
         if you bumped the chunking or normalization algorithm.\n\
         This is irreversible (the existing corpus tree is replaced).\n\
         Type 'yes' to continue: "
    };
    if !yes && !confirm(prompt)? {
        println!("aborted; no changes written");
        return Ok(());
    }

    let run_params = bookrack_ingest::rebuild::RebuildParams {
        only: book,
        stale_only,
        dry_run: false,
        ..Default::default()
    };
    let report = bookrack_ingest::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &run_params)
        .context("rebuild")?;
    println!(
        "rebuilt: {} intakes ({} missing_envelope, {} mismatched, {} failed)",
        report.rebuilt.len(),
        report.missing_envelope.len(),
        report.mismatched.len(),
        report.failed.len()
    );

    // L0-only rebuilds end here with a fresh node tree but no
    // index_meta stamps; the next `query` would fail at the
    // serve-side gate. Re-stamp from the existing chunks before
    // returning so search keeps working. When `--include-vectors`
    // is set the subsequent reembed writes the same stamps, so this
    // path is skipped to avoid a redundant reconcile.
    if !include_vectors && !report.rebuilt.is_empty() {
        let lancedb_dir = cfg.lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let stamped = bookrack_ingest::rebuild::stamp_index_from_existing_chunks(
            &corpus,
            &lancedb_dir,
            &embed_cfg.model,
        )
        .await
        .context("stamp index_meta after rebuild")?;
        if !stamped {
            println!(
                "no chunks on disk; index_meta stamps were not written. \
                 Run `bookrack vectors reembed` after embedding to enable search."
            );
        }
    }

    if include_vectors && !report.rebuilt.is_empty() {
        let lancedb_dir = cfg.lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let embedder_client = embedder(cfg, &embed_cfg)?;
        let _ = profile_name;
        let reembed = bookrack_ingest::reembed::reembed_all(
            &catalog,
            &corpus,
            &lancedb_dir,
            &embed_cfg,
            &embedder_client,
            book,
            stale_only,
        )
        .await
        .context("reembed after rebuild")?;
        let total_written: usize = reembed
            .intakes
            .iter()
            .map(|o| o.embed_run.chunks_written)
            .sum();
        println!(
            "reembedded: {} intakes / {} chunks",
            reembed.intakes.len(),
            total_written
        );
    }
    Ok(())
}
