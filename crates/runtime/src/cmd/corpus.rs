// SPDX-License-Identifier: Apache-2.0

//! `bookrack corpus rebuild` — regenerate the corpus tree from the
//! opaque envelopes, optionally re-embedding.
//!
//! Two entry-point shapes coexist:
//!
//! - [`rebuild`] is the in-process path used by the local REPL and
//!   the legacy daemon fallback: it computes a plan, prompts via an
//!   `ask` closure, then runs the execute step. The execute step
//!   re-derives the target set from the current catalog, so any
//!   drift between the prompt and the execute lands in the
//!   destructive operation.
//! - [`plan_rebuild`] + [`execute_rebuild_from_plan`] are the
//!   two-step path the control-plane handler drives: the operator
//!   confirms the dry-run leg, and the execute leg acts on the
//!   exact pinned target set the plan recorded — drift cannot leak
//!   between the two legs because the execute does not re-derive.

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;

use crate::embed_helpers::embedder;

#[allow(clippy::too_many_arguments)]
pub async fn rebuild<F>(
    cfg: &Config,
    include_vectors: bool,
    book: Option<i64>,
    stale_only: bool,
    dry_run: bool,
    yes: bool,
    profile_name: Option<&str>,
    ask: F,
) -> Result<()>
where
    F: FnOnce(&str) -> Result<bool>,
{
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
    if !yes && !ask(prompt)? {
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
            None,
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

/// Compute a rebuild plan without writing anything. Returns the
/// classification buckets a subsequent [`execute_rebuild_from_plan`]
/// call will pin to.
pub fn plan_rebuild(
    cfg: &Config,
    book: Option<i64>,
    stale_only: bool,
) -> Result<bookrack_ingest::rebuild::RebuildReport> {
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let params = bookrack_ingest::rebuild::RebuildParams {
        only: book,
        stale_only,
        dry_run: true,
        ..Default::default()
    };
    bookrack_ingest::rebuild::rebuild_from_intakes(&mut corpus, &catalog, &params)
        .context("plan rebuild")
}

/// Aggregate outcome of [`execute_rebuild_from_plan`]: the rebuild
/// classification plus any L0 stamp / L2 reembed sidecar work the
/// execute leg performed.
#[derive(Debug, Clone, Default)]
pub struct ExecuteRebuildOutcome {
    pub report: bookrack_ingest::rebuild::RebuildReport,
    /// `Some(true)` if `stamp_index_from_existing_chunks` wrote
    /// stamps; `Some(false)` if the chunks table was empty so no
    /// stamping happened; `None` when the `include_vectors` path
    /// took precedence and stamping was skipped intentionally.
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

/// Execute a rebuild against the exact pinned set computed by an
/// earlier [`plan_rebuild`] call. Strict: every id in `pinned_ids`
/// must still resolve to a rebuildable catalog row, else the call
/// aborts without writing.
pub async fn execute_rebuild_from_plan(
    cfg: &Config,
    pinned_ids: Vec<i64>,
    include_vectors: bool,
) -> Result<ExecuteRebuildOutcome> {
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;

    let report = bookrack_ingest::rebuild::rebuild_from_intakes(
        &mut corpus,
        &catalog,
        &bookrack_ingest::rebuild::RebuildParams {
            only_ids: Some(pinned_ids),
            dry_run: false,
            ..Default::default()
        },
    )
    .context("rebuild")?;

    let mut outcome = ExecuteRebuildOutcome {
        report,
        ..Default::default()
    };
    if outcome.report.rebuilt.is_empty() {
        return Ok(outcome);
    }

    if include_vectors {
        let lancedb_dir = cfg.lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let embedder_client = embedder(cfg, &embed_cfg)?;
        let reembed = bookrack_ingest::reembed::reembed_all(
            &catalog,
            &corpus,
            &lancedb_dir,
            &embed_cfg,
            &embedder_client,
            None,
            Some(&outcome.report.rebuilt),
            false,
        )
        .await
        .context("reembed after rebuild")?;
        let chunks_written: usize = reembed
            .intakes
            .iter()
            .map(|o| o.embed_run.chunks_written)
            .sum();
        outcome.reembed = Some(ReembedSummary {
            intakes: reembed.intakes.len(),
            chunks_written,
        });
    } else {
        let lancedb_dir = cfg.lancedb_dir();
        let embed_cfg = EmbedConfig::from_env();
        let stamped = bookrack_ingest::rebuild::stamp_index_from_existing_chunks(
            &corpus,
            &lancedb_dir,
            &embed_cfg.model,
        )
        .await
        .context("stamp index_meta after rebuild")?;
        outcome.stamped_from_existing_chunks = Some(stamped);
    }
    Ok(outcome)
}
