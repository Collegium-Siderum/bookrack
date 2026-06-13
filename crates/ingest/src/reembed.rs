// SPDX-License-Identifier: Apache-2.0

//! L2 reembed: rebuild the dense store from the chunks already on disk,
//! without re-extracting or re-chunking any source file.
//!
//! Each book's existing [`ChunkRow`]s are read back via
//! [`ChunkStore::scan_partition`], mapped to [`ChunkPlan`]s by dropping
//! the vector column, and fed back through [`embed_book_chunks`]. The
//! latter owns the delete-then-append / churn-bump / cold-start ANN
//! decisions, so reembed inherits them automatically — stamps mismatch
//! handling is the same.

use std::path::Path;
use std::time::Instant;

use bookrack_catalog::{Catalog, Intake, IntakeStatus};
use bookrack_config::EmbedConfig;
use bookrack_core::{PartitionIdx, error_chain};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_extract::EXTRACTOR_VERSION;
use bookrack_vectors::{ChunkRow, ChunkStore};

use crate::chunk::ChunkPlan;
use crate::embed_run::{EmbedRunReport, embed_book_chunks};
use crate::{IngestError, Result, audit_as, maintenance_run_id};

/// A planned per-book reembed: what would happen if [`reembed_book`]
/// ran on this `intake_id`.
#[derive(Debug, Clone)]
pub struct ReembedPlan {
    pub intake_id: i64,
    pub partition: PartitionIdx,
    pub chunk_count: usize,
    pub total_chars: usize,
}

/// What one reembed call produced for one intake.
#[derive(Debug, Clone)]
pub struct ReembedOutcome {
    pub intake_id: i64,
    pub embed_run: EmbedRunReport,
}

/// Aggregate report for [`reembed_all`].
#[derive(Debug, Clone, Default)]
pub struct ReembedReport {
    pub intakes: Vec<ReembedOutcome>,
    /// Intakes the driver skipped because their partition held no
    /// chunks (e.g. an aborted prior embed). Not an error.
    pub skipped_empty: Vec<i64>,
}

/// Build a [`ReembedPlan`] for each intake currently in
/// [`IntakeStatus::Embedded`], or for `only` when set. Reads the chunks
/// table but writes nothing.
///
/// When `stale_only` is true the target set is restricted to intakes
/// whose stored `extractor_version` does not equal this binary's
/// [`EXTRACTOR_VERSION`] — the same filter [`reembed_all`] applies, so
/// the printed plan and the eventual run always agree.
pub async fn plan_reembed(
    catalog: &Catalog,
    lancedb_dir: &Path,
    only: Option<i64>,
    stale_only: bool,
) -> Result<Vec<ReembedPlan>> {
    // The on-disk schema decides the dim; passing 0 forces the open
    // path to read it from the schema for an existing table.
    let store = ChunkStore::open(lancedb_dir, 0).await?;
    let mut targets = collect_targets(catalog, only)?;
    if stale_only {
        let stale: std::collections::HashSet<i64> = catalog
            .stale_partitions(EXTRACTOR_VERSION)
            .map_err(IngestError::from)?
            .into_iter()
            .collect();
        targets.retain(|intake| stale.contains(&intake.intake_id));
    }
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

/// Reembed one book: read its rows back, drop their vectors, and run
/// [`embed_book_chunks`] on the resulting [`ChunkPlan`]s. Returns the
/// underlying [`EmbedRunReport`].
///
/// On an empty partition returns a zeroed report — the caller may treat
/// it as a skip.
pub async fn reembed_book<E: Embedder>(
    intake_id: i64,
    embedder: &E,
    corpus: &Corpus,
    lancedb_dir: &Path,
    cfg: &EmbedConfig,
) -> Result<EmbedRunReport> {
    let partition = PartitionIdx::new(intake_id);
    let plans = read_chunk_plans(lancedb_dir, partition).await?;
    if plans.is_empty() {
        return Ok(EmbedRunReport::default());
    }
    embed_book_chunks(&plans, embedder, corpus, lancedb_dir, cfg).await
}

/// Reembed every intake currently in [`IntakeStatus::Embedded`], or
/// just `only` when set. Per-book failures abort the whole run so the
/// caller can surface the first error verbatim.
///
/// When `stale_only` is true the target set is further restricted to
/// intakes whose stored `extractor_version` does not equal this
/// binary's [`EXTRACTOR_VERSION`]; combines with `only` by
/// intersection.
pub async fn reembed_all<E: Embedder>(
    catalog: &Catalog,
    corpus: &Corpus,
    lancedb_dir: &Path,
    cfg: &EmbedConfig,
    embedder: &E,
    only: Option<i64>,
    stale_only: bool,
) -> Result<ReembedReport> {
    let mut targets = collect_targets(catalog, only)?;
    if stale_only {
        let stale: std::collections::HashSet<i64> = catalog
            .stale_partitions(EXTRACTOR_VERSION)
            .map_err(IngestError::from)?
            .into_iter()
            .collect();
        targets.retain(|intake| stale.contains(&intake.intake_id));
    }
    let run_id = maintenance_run_id("reembed");
    let mut report = ReembedReport::default();
    for intake in targets {
        let intake_id = intake.intake_id;
        let sha = intake.source_sha256.as_str();
        let book_root_raw = PartitionIdx::new(intake_id).root().get();
        let started = Instant::now();
        let embed_run = match reembed_book(intake_id, embedder, corpus, lancedb_dir, cfg).await {
            Ok(r) => r,
            Err(e) => {
                audit_as(
                    catalog,
                    "reembed",
                    &run_id,
                    sha,
                    Some(book_root_raw),
                    "embed",
                    "embed",
                    "fail",
                    started,
                    None,
                    Some(&error_chain(&e)),
                );
                return Err(e);
            }
        };
        if embed_run.chunks_written == 0 {
            report.skipped_empty.push(intake_id);
            continue;
        }
        audit_as(
            catalog,
            "reembed",
            &run_id,
            sha,
            Some(book_root_raw),
            "embed",
            "embed",
            "ok",
            started,
            Some(format!(
                r#"{{"chunks":{},"batches":{},"shrink_events":{},"chars":{}}}"#,
                embed_run.chunks_written,
                embed_run.batches,
                embed_run.shrink_events,
                embed_run.total_chars
            )),
            None,
        );
        report.intakes.push(ReembedOutcome {
            intake_id,
            embed_run,
        });
    }
    Ok(report)
}

fn collect_targets(catalog: &Catalog, only: Option<i64>) -> Result<Vec<Intake>> {
    Ok(match only {
        Some(id) => {
            let intake = catalog
                .intake_by_id(id)
                .map_err(IngestError::from)?
                .ok_or(IngestError::UnknownIntake(id))?;
            if intake.status != IntakeStatus::Embedded {
                return Err(IngestError::IntakeNotEmbedded(id));
            }
            vec![intake]
        }
        None => catalog
            .intakes_with_status(IntakeStatus::Embedded)
            .map_err(IngestError::from)?,
    })
}

/// Open the store, scan the partition, drop the vector column.
async fn read_chunk_plans(lancedb_dir: &Path, partition: PartitionIdx) -> Result<Vec<ChunkPlan>> {
    // Dim hint is irrelevant for an existing table: open reads the
    // schema. For a fresh directory the scan is empty.
    let store = ChunkStore::open(lancedb_dir, 0).await?;
    let rows = store.scan_partition(partition).await?;
    Ok(rows.into_iter().map(row_to_plan).collect())
}

fn row_to_plan(row: ChunkRow) -> ChunkPlan {
    ChunkPlan {
        start_node_id: row.start_node_id,
        start_char_offset: row.start_char_offset,
        end_node_id: row.end_node_id,
        end_char_offset: row.end_char_offset,
        text: row.text,
        norm_chunk_sha256: row.norm_chunk_sha256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    use bookrack_catalog::NewIntake;
    use bookrack_core::ItemKind;
    use bookrack_embed::{EmbedError, Embedder, Result as EmbedResult};
    use bookrack_vectors::ChunkRow;

    use crate::current_index_stamps;

    /// A toy embedder whose vector encodes its call generation, so the
    /// reembed test can prove the vectors changed.
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
                let _ = EmbedError::Unreachable("".to_string());
                Ok::<Vec<Vec<f32>>, EmbedError>(
                    (0..n)
                        .map(|_| {
                            let mut v = vec![0.0f32; 4];
                            v[1] = generation as f32;
                            v
                        })
                        .collect(),
                )
            }
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
                    ItemKind::Book,
                    &NewIntake::new(format!("sha-{id}"))
                        .format("txt")
                        .byte_size(1),
                )
                .expect("register");
            assert_eq!(reg.intake().intake_id, id);
            catalog
                .set_intake_status(ItemKind::Book, id, IntakeStatus::Embedded)
                .expect("status");
        }
    }

    fn stamp_corpus(corpus: &mut Corpus, model: &str, dim: u32) {
        let stamps = current_index_stamps(model, dim);
        corpus.reconcile_index_stamps(&stamps).expect("reconcile");
    }

    fn embed_cfg(model: &str) -> EmbedConfig {
        EmbedConfig {
            model: model.to_string(),
            ..EmbedConfig::from_env()
        }
    }

    #[tokio::test]
    async fn plan_reembed_lists_only_embedded_intakes_with_chunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        seed_catalog_embedded(&mut catalog, &[1, 2]);
        seed_partition(dir.path(), 1, 3).await;
        // Partition 2 left empty: planner skips it.

        let plans = plan_reembed(&catalog, dir.path(), None, false)
            .await
            .expect("plan");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].intake_id, 1);
        assert_eq!(plans[0].chunk_count, 3);
    }

    #[tokio::test]
    async fn plan_reembed_respects_stale_only() {
        // Both intakes carry chunks, but only one has a stale
        // extractor_version. With `stale_only = true` the plan must
        // match what `reembed_all(stale_only = true)` will actually run.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        seed_catalog_embedded(&mut catalog, &[1, 2]);
        seed_partition(dir.path(), 1, 3).await;
        seed_partition(dir.path(), 2, 2).await;
        // Pin intake 1 at the current `EXTRACTOR_VERSION` so the
        // default-zero state cannot be mistaken for stale, then mark
        // intake 2 stale by stamping a version below it.
        catalog
            .set_extraction(ItemKind::Book, 1, "fake", EXTRACTOR_VERSION)
            .expect("pin extractor_version on intake 1");
        catalog
            .set_extraction(
                ItemKind::Book,
                2,
                "fake",
                EXTRACTOR_VERSION.saturating_sub(1),
            )
            .expect("override extractor_version on intake 2");

        let full = plan_reembed(&catalog, dir.path(), None, false)
            .await
            .expect("plan full");
        assert_eq!(full.len(), 2);

        let stale = plan_reembed(&catalog, dir.path(), None, true)
            .await
            .expect("plan stale");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].intake_id, 2);
    }

    #[tokio::test]
    async fn reembed_book_replaces_vectors_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        stamp_corpus(&mut corpus, "fake-1", 4);
        seed_catalog_embedded(&mut catalog, &[1]);
        seed_partition(dir.path(), 1, 5).await;

        let before = ChunkStore::open(dir.path(), 4)
            .await
            .expect("open")
            .count_rows()
            .await
            .expect("count");
        assert_eq!(before, 5);

        let cfg = embed_cfg("fake-1");
        let report = reembed_book(1, &Fake { generation: 7 }, &corpus, dir.path(), &cfg)
            .await
            .expect("reembed");
        assert_eq!(report.chunks_written, 5);

        let store = ChunkStore::open(dir.path(), 4).await.expect("open");
        assert_eq!(store.count_rows().await.expect("count"), 5);
        let rows = store
            .scan_partition(PartitionIdx::new(1))
            .await
            .expect("scan");
        for row in &rows {
            // Generation byte landed in slot 1 — proves the rows were
            // rewritten rather than left untouched.
            assert_eq!(row.vector[1], 7.0);
        }
    }

    #[tokio::test]
    async fn reembed_all_visits_every_embedded_intake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        stamp_corpus(&mut corpus, "fake-1", 4);
        seed_catalog_embedded(&mut catalog, &[1, 2, 3]);
        seed_partition(dir.path(), 1, 2).await;
        seed_partition(dir.path(), 2, 4).await;
        // intake 3 has no chunks: should land in skipped_empty.

        let cfg = embed_cfg("fake-1");
        let report = reembed_all(
            &catalog,
            &corpus,
            dir.path(),
            &cfg,
            &Fake { generation: 9 },
            None,
            false,
        )
        .await
        .expect("reembed_all");

        assert_eq!(report.intakes.len(), 2);

        // Each reembedded book gets an embed-ok trail row; the whole
        // pass shares one run id. The chunkless intake records nothing.
        let mut run_ids = std::collections::HashSet::new();
        for id in [1, 2] {
            let rows = catalog
                .pipeline_audit_for_book(PartitionIdx::new(id).root().get())
                .expect("trail");
            let last = rows.last().expect("trail rows");
            assert_eq!(last.stage, "embed");
            assert_eq!(last.outcome, "ok");
            assert_eq!(last.actor_detail.as_deref(), Some("reembed"));
            assert!(last.pipeline_run_id.starts_with("reembed-"));
            run_ids.insert(last.pipeline_run_id.clone());
        }
        assert_eq!(run_ids.len(), 1, "one invocation, one run id");
        assert!(
            catalog
                .pipeline_audit_for_book(PartitionIdx::new(3).root().get())
                .expect("trail")
                .is_empty()
        );

        let ids: Vec<i64> = report.intakes.iter().map(|o| o.intake_id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(report.skipped_empty, vec![3]);
    }
}
