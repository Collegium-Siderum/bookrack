// SPDX-License-Identifier: Apache-2.0

//! Paper-side reembed. Peer of `bookrack_ingest::reembed` for the paper
//! pipeline: takes the chunk rows currently on disk in `lancedb_papers`,
//! re-embeds them under the active embedder, and writes the new vectors
//! back. The corpus node tree is not touched — only the chunks table.

use std::path::Path;
use std::time::Instant;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::EmbedConfig;
use bookrack_core::{NodeId, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_extract::EXTRACTOR_VERSION;
use bookrack_vectors::ChunkStore;

use crate::{GleanError, PlannedChunk, Result, audit_as, embed_and_write_chunks, new_run_id};

/// A planned per-paper reembed: what would happen if [`reembed_paper`]
/// ran on this `intake_id`.
#[derive(Debug, Clone)]
pub struct ReembedPlan {
    pub intake_id: i64,
    pub partition: PartitionIdx,
    pub chunk_count: usize,
    pub total_chars: usize,
}

/// What one [`reembed_paper`] call produced for one intake.
#[derive(Debug, Clone)]
pub struct ReembedOutcome {
    pub intake_id: i64,
    pub chunks_written: usize,
}

/// Aggregate report for [`reembed_all`].
#[derive(Debug, Clone, Default)]
pub struct ReembedReport {
    pub intakes: Vec<ReembedOutcome>,
    /// Intakes the driver skipped because their partition held no
    /// chunks (e.g. an aborted prior embed). Not an error.
    pub skipped_empty: Vec<i64>,
}

/// Build a [`ReembedPlan`] for each `Embedded` paper intake, or for
/// `only` when set. Reads `lancedb_papers` but writes nothing.
///
/// When `stale_only` is true the target set is restricted to intakes
/// whose stored `extractor_version` does not equal [`EXTRACTOR_VERSION`].
///
/// When `only_ids` is `Some`, the target set is exactly that list —
/// `only` and `stale_only` are ignored. Each id must resolve to an
/// existing catalog row in [`IntakeStatus::Embedded`]; any unknown
/// or non-embedded id aborts with [`GleanError::UnknownIntake`] /
/// [`GleanError::IntakeNotRebuildable`]. Used by destructive RPCs
/// to pin the execute leg to the dry-run leg's confirmed set.
pub async fn plan_reembed(
    catalog: &Catalog,
    lancedb_dir: &Path,
    only: Option<i64>,
    only_ids: Option<&[i64]>,
    stale_only: bool,
) -> Result<Vec<ReembedPlan>> {
    // Passing 0 forces the open path to read dim from the schema for an
    // existing table.
    let store = ChunkStore::open(lancedb_dir, 0).await?;
    let targets = if let Some(ids) = only_ids {
        collect_pinned_targets(catalog, ids)?
    } else {
        let mut t = collect_targets(catalog, only)?;
        if stale_only {
            let stale: std::collections::HashSet<i64> = catalog
                .stale_partitions(EXTRACTOR_VERSION)?
                .into_iter()
                .collect();
            t.retain(|intake| stale.contains(&intake.intake_id));
        }
        t
    };
    let mut plans = Vec::new();
    for intake in targets {
        let intake_id = intake.intake_id;
        let partition = PartitionIdx::new(intake_id);
        let rows = store.scan_partition(partition).await?;
        if rows.is_empty() {
            continue;
        }
        let total_chars = rows.iter().map(|r| r.text.chars().count()).sum();
        plans.push(ReembedPlan {
            intake_id,
            partition,
            chunk_count: rows.len(),
            total_chars,
        });
    }
    Ok(plans)
}

/// Reembed one paper intake: scan its partition, re-embed every row,
/// and replace the partition in place. Returns the number of rows
/// written; an empty partition returns `0` for the caller to interpret
/// as a skip.
pub async fn reembed_paper<E: Embedder>(
    intake_id: i64,
    embedder: &E,
    corpus: &mut Corpus,
    lancedb_dir: &Path,
    cfg: &EmbedConfig,
) -> Result<usize> {
    let plans = read_chunk_plans(lancedb_dir, PartitionIdx::new(intake_id)).await?;
    if plans.is_empty() {
        return Ok(0);
    }
    embed_and_write_chunks(corpus, lancedb_dir, embedder, cfg, intake_id, &plans).await
}

/// Reembed every paper intake at [`IntakeStatus::Embedded`], or just
/// `only` when set. Per-intake failures abort the whole run so the
/// caller can surface the first error verbatim.
///
/// When `stale_only` is true the target set is further restricted to
/// intakes whose stored `extractor_version` does not equal
/// [`EXTRACTOR_VERSION`]; combines with `only` by intersection.
///
/// When `only_ids` is `Some`, the target set is exactly that list —
/// `only` and `stale_only` are ignored. Each id must resolve to an
/// existing catalog row in [`IntakeStatus::Embedded`]; any unknown
/// or non-embedded id aborts with [`GleanError::UnknownIntake`] /
/// [`GleanError::IntakeNotRebuildable`]. Used by destructive RPCs
/// to pin the execute leg to the dry-run leg's confirmed set.
#[allow(clippy::too_many_arguments)]
pub async fn reembed_all<E: Embedder>(
    catalog: &Catalog,
    corpus: &mut Corpus,
    lancedb_dir: &Path,
    cfg: &EmbedConfig,
    embedder: &E,
    only: Option<i64>,
    only_ids: Option<&[i64]>,
    stale_only: bool,
) -> Result<ReembedReport> {
    let targets = if let Some(ids) = only_ids {
        collect_pinned_targets(catalog, ids)?
    } else {
        let mut t = collect_targets(catalog, only)?;
        if stale_only {
            let stale: std::collections::HashSet<i64> = catalog
                .stale_partitions(EXTRACTOR_VERSION)?
                .into_iter()
                .collect();
            t.retain(|intake| stale.contains(&intake.intake_id));
        }
        t
    };
    let run_id = new_run_id("reembed");
    let mut report = ReembedReport::default();
    for intake in targets {
        let intake_id = intake.intake_id;
        let sha = intake.source_sha256.as_str();
        let work_root_raw = PartitionIdx::new(intake_id).root().get();
        let started = Instant::now();
        let chunks_written =
            match reembed_paper(intake_id, embedder, corpus, lancedb_dir, cfg).await {
                Ok(n) => n,
                Err(e) => {
                    audit_as(
                        catalog,
                        "glean-reembed",
                        &run_id,
                        sha,
                        Some(work_root_raw),
                        "embed",
                        "embed",
                        "fail",
                        started,
                        None,
                        Some(&e.to_string()),
                    );
                    return Err(e);
                }
            };
        if chunks_written == 0 {
            report.skipped_empty.push(intake_id);
            continue;
        }
        audit_as(
            catalog,
            "glean-reembed",
            &run_id,
            sha,
            Some(work_root_raw),
            "embed",
            "embed",
            "ok",
            started,
            Some(format!(r#"{{"chunks":{chunks_written}}}"#)),
            None,
        );
        report.intakes.push(ReembedOutcome {
            intake_id,
            chunks_written,
        });
    }
    Ok(report)
}

fn collect_targets(catalog: &Catalog, only: Option<i64>) -> Result<Vec<bookrack_catalog::Intake>> {
    Ok(match only {
        Some(id) => {
            let intake = catalog
                .intake_by_id(id)?
                .ok_or(GleanError::UnknownIntake(id))?;
            if intake.status != IntakeStatus::Embedded {
                return Err(GleanError::IntakeNotRebuildable(id));
            }
            vec![intake]
        }
        None => catalog.intakes_with_status(IntakeStatus::Embedded)?,
    })
}

fn collect_pinned_targets(catalog: &Catalog, ids: &[i64]) -> Result<Vec<bookrack_catalog::Intake>> {
    ids.iter()
        .map(|id| {
            let intake = catalog
                .intake_by_id(*id)?
                .ok_or(GleanError::UnknownIntake(*id))?;
            if intake.status != IntakeStatus::Embedded {
                return Err(GleanError::IntakeNotRebuildable(*id));
            }
            Ok(intake)
        })
        .collect()
}

async fn read_chunk_plans(
    lancedb_dir: &Path,
    partition: PartitionIdx,
) -> Result<Vec<PlannedChunk>> {
    let store = ChunkStore::open(lancedb_dir, 0).await?;
    let rows = store.scan_partition(partition).await?;
    Ok(rows
        .into_iter()
        .map(|row| PlannedChunk {
            start_node_id: NodeId::new(row.start_node_id.get()),
            start_char_offset: row.start_char_offset,
            end_node_id: NodeId::new(row.end_node_id.get()),
            end_char_offset: row.end_char_offset,
            text: row.text,
            norm_chunk_sha256: row.norm_chunk_sha256,
        })
        .collect())
}
