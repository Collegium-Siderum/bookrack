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

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    use bookrack_catalog::NewIntake;
    use bookrack_core::ItemKind;
    use bookrack_embed::{EmbedError, Result as EmbedResult};
    use bookrack_vectors::ChunkRow;

    /// A toy embedder whose vector encodes its call generation, so
    /// tests can prove the vectors changed.
    struct Fake {
        generation: u8,
    }

    impl Embedder for Fake {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let n = texts.len();
            let generation = self.generation;
            async move {
                Ok((0..n)
                    .map(|_| {
                        let mut v = vec![0.0f32; 4];
                        v[1] = generation as f32;
                        v
                    })
                    .collect())
            }
        }
    }

    /// A fake embedder that always fails, forcing the abort path.
    struct Offline;

    impl Embedder for Offline {
        fn embed_batch(
            &self,
            _texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            std::future::ready(Err(EmbedError::Unreachable(
                "test embedder offline".to_string(),
            )))
        }
    }

    fn fake_row(intake_id: i64, offset: i64, text: &str) -> ChunkRow {
        let node = PartitionIdx::new(intake_id)
            .node_id(offset)
            .expect("offset in range");
        ChunkRow {
            vector: vec![0.0; 4],
            text: text.to_string(),
            start_node_id: node,
            start_char_offset: 0,
            end_node_id: node,
            end_char_offset: text.len() as i32,
            norm_chunk_sha256: format!("sha-p{intake_id}-o{offset}"),
        }
    }

    async fn seed_partition(lancedb_dir: &Path, intake_id: i64, count: usize) {
        let store = ChunkStore::open(lancedb_dir, 4).await.expect("open");
        let rows: Vec<ChunkRow> = (0..count as i64)
            .map(|o| fake_row(intake_id, o + 1, &format!("chunk {intake_id}-{o}")))
            .collect();
        store.append(&rows).await.expect("seed");
    }

    fn seed_catalog_embedded(catalog: &mut Catalog, intake_ids: &[i64]) {
        for &id in intake_ids {
            let reg = catalog
                .register_intake(
                    ItemKind::Paper,
                    &NewIntake::new(format!("sha-{id}")).format("pdf".to_string()),
                )
                .expect("register");
            assert_eq!(reg.intake().intake_id, id);
            catalog
                .set_intake_status(ItemKind::Paper, id, IntakeStatus::Embedded)
                .expect("status");
        }
    }

    fn embed_cfg(model: &str) -> EmbedConfig {
        EmbedConfig {
            model: model.to_string(),
            ..EmbedConfig::default()
        }
    }

    async fn partition_tags(lancedb_dir: &Path, intake_id: i64) -> Vec<f32> {
        let store = ChunkStore::open(lancedb_dir, 4).await.expect("open");
        store
            .scan_partition(PartitionIdx::new(intake_id))
            .await
            .expect("scan")
            .iter()
            .map(|row| row.vector[1])
            .collect()
    }

    #[tokio::test]
    async fn plan_reembed_aborts_on_unknown_and_non_embedded_pinned_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        seed_catalog_embedded(&mut catalog, &[1]);
        seed_partition(dir.path(), 1, 3).await;

        let err = plan_reembed(&catalog, dir.path(), None, Some(&[1, 9_999]), false)
            .await
            .expect_err("an unknown pinned id must abort");
        assert!(
            matches!(err, GleanError::UnknownIntake(9_999)),
            "got {err:?}"
        );

        // A known id outside `Embedded` aborts too.
        let pending = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("sha-pending".to_string()).format("pdf".to_string()),
            )
            .expect("register")
            .intake()
            .intake_id;
        let err = plan_reembed(&catalog, dir.path(), None, Some(&[1, pending]), false)
            .await
            .expect_err("a non-embedded pinned id must abort");
        match err {
            GleanError::IntakeNotRebuildable(id) => assert_eq!(id, pending),
            other => panic!("expected IntakeNotRebuildable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reembed_all_only_ids_pins_the_set_and_ignores_only_and_stale_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        seed_catalog_embedded(&mut catalog, &[1, 2]);
        seed_partition(dir.path(), 1, 3).await;
        seed_partition(dir.path(), 2, 2).await;

        // `only` names an unknown id and `stale_only` is on: were
        // either consulted, the call would abort or filter the pinned
        // target away. The pinned list alone decides.
        let report = reembed_all(
            &catalog,
            &mut corpus,
            dir.path(),
            &embed_cfg("fake-1"),
            &Fake { generation: 7 },
            Some(9_999),
            Some(&[1]),
            true,
        )
        .await
        .expect("reembed");
        assert_eq!(report.intakes.len(), 1);
        assert_eq!(report.intakes[0].intake_id, 1);
        assert_eq!(report.intakes[0].chunks_written, 3);

        // The pinned partition was rewritten; the unpinned one was not.
        assert!(
            partition_tags(dir.path(), 1)
                .await
                .iter()
                .all(|&t| t == 7.0)
        );
        assert!(
            partition_tags(dir.path(), 2)
                .await
                .iter()
                .all(|&t| t == 0.0)
        );
    }

    #[tokio::test]
    async fn reembed_all_aborts_on_embed_failure_and_audits_the_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        seed_catalog_embedded(&mut catalog, &[1]);
        seed_partition(dir.path(), 1, 3).await;

        let err = reembed_all(
            &catalog,
            &mut corpus,
            dir.path(),
            &embed_cfg("fake-1"),
            &Offline,
            None,
            None,
            false,
        )
        .await
        .expect_err("the offline embedder must abort the run");
        assert!(matches!(err, GleanError::Embed(_)), "got {err:?}");

        // The failure landed as an audit row on the partition root.
        let work_root_raw = PartitionIdx::new(1).root().get();
        let rows = catalog
            .pipeline_audit_for_book(work_root_raw)
            .expect("audit rows");
        assert!(
            rows.iter().any(|r| {
                r.stage == "embed"
                    && r.outcome == "fail"
                    && r.actor_detail.as_deref() == Some("glean-reembed")
            }),
            "expected a glean-reembed fail row, got {rows:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_partition_is_skipped_and_planning_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        seed_catalog_embedded(&mut catalog, &[1, 2]);
        // Partition 1 left empty; partition 2 carries chunks.
        seed_partition(dir.path(), 2, 2).await;

        // The planner lists only the non-empty partition and leaves
        // the stored vectors untouched.
        let plans = plan_reembed(&catalog, dir.path(), None, None, false)
            .await
            .expect("plan");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].intake_id, 2);
        assert_eq!(plans[0].chunk_count, 2);
        assert!(
            partition_tags(dir.path(), 2)
                .await
                .iter()
                .all(|&t| t == 0.0)
        );

        // The driver buckets the empty partition as skipped, not failed.
        let report = reembed_all(
            &catalog,
            &mut corpus,
            dir.path(),
            &embed_cfg("fake-1"),
            &Fake { generation: 7 },
            None,
            None,
            false,
        )
        .await
        .expect("reembed");
        assert_eq!(report.skipped_empty, vec![1]);
        assert_eq!(report.intakes.len(), 1);
        assert_eq!(report.intakes[0].intake_id, 2);
    }
}
