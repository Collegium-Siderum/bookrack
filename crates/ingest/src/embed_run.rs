// SPDX-License-Identifier: Apache-2.0

//! The EMBED stage: turn a book's [`ChunkPlan`]s into vectors and write
//! them to the dense store.
//!
//! Batching is sequential and greedy: chunks are packed into a batch up
//! to a character budget and a hard chunk cap, each batch is embedded in
//! one request, and the resulting rows are appended to the store. An
//! overloaded server (HTTP 5xx) is met by halving the batch and retrying
//! each half, down to a single chunk — the producer/consumer overlap and
//! AIMD scheduling the config leaves room for are deferred.
//!
//! Writing a book is delete-then-append on its partition, so re-ingesting
//! the same file replaces its rows rather than duplicating them.

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bookrack_config::EmbedConfig;
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_vectors::{AnnConfig, AnnKind, ChunkRow, ChunkStore};

use crate::chunk::ChunkPlan;
use crate::{IngestError, Result, current_index_stamps};

/// What one EMBED run produced — the row count plus the batching metrics
/// that diagnose how the run behaved against the server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedRunReport {
    /// Rows written to the vector store.
    pub chunks_written: usize,
    /// Embed requests issued, after any overload shrinking.
    pub batches: usize,
    /// Times an overloaded server forced a batch to be halved.
    pub shrink_events: usize,
    /// Total characters of chunk text embedded.
    pub total_chars: usize,
}

/// Embed a book's chunk plans and write them to the store under
/// `lancedb_dir`. Returns the run's [`EmbedRunReport`].
///
/// The embedding dimension is probed from the first chunk and the store
/// is opened fixed to it. The book's prior rows are cleared before the
/// first append, keying the deletion on the partition the chunks live in.
pub async fn embed_book_chunks<E: Embedder>(
    plans: &[ChunkPlan],
    embedder: &E,
    corpus: &Corpus,
    lancedb_dir: &Path,
    cfg: &EmbedConfig,
) -> Result<EmbedRunReport> {
    if plans.is_empty() {
        return Ok(EmbedRunReport::default());
    }

    // Probe the dimension before opening the store: the store fixes its
    // dimension on creation, and only the model knows it.
    let probe = embedder
        .embed_batch(std::slice::from_ref(&plans[0].text))
        .await
        .map_err(IngestError::Embed)?;
    let dim = probe
        .first()
        .map(Vec::len)
        .filter(|&d| d > 0)
        .ok_or(IngestError::EmptyEmbedding)?;

    // Stamp the build parameters on a fresh index, or refuse a book whose
    // model or algorithm version differs from the one the index was built
    // with — before any vector is written, so the index is never mixed.
    corpus.reconcile_index_stamps(&current_index_stamps(&cfg.model, dim as u32))?;

    let store = ChunkStore::open(lancedb_dir, dim).await?;
    let rows_before_delete = store.count_rows().await?;
    store
        .delete_partition(plans[0].start_node_id.partition())
        .await?;
    let rows_after_delete = store.count_rows().await?;
    let deleted = rows_before_delete.saturating_sub(rows_after_delete);

    let mut report = EmbedRunReport::default();
    let mut start = 0usize;
    while start < plans.len() {
        let end = greedy_batch_end(plans, start, cfg);
        let batch = &plans[start..end];
        let batch_chars: usize = batch.iter().map(|p| p.text.chars().count()).sum();
        tracing::debug!(chunks = batch.len(), chars = batch_chars, "embedding batch");

        let started = Instant::now();
        let (vectors, shrinks) = embed_with_shrink(embedder, batch, 0).await?;
        tracing::debug!(
            vectors = vectors.len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1e3,
            "batch embedded"
        );

        let rows = store.append(&to_rows(batch, vectors)).await?;
        tracing::debug!(rows, "appended batch to store");

        report.chunks_written += rows;
        report.batches += 1;
        report.shrink_events += shrinks;
        report.total_chars += batch_chars;
        start = end;
    }

    // Drain the delete-then-append churn: compact fragments, prune old
    // versions, and absorb the freshly-appended rows into any existing
    // index. Failure is non-fatal — the table is still consistent, just
    // carrying tombstones and unindexed rows the next run will pick up.
    if let Err(e) = store.optimize().await {
        tracing::warn!(error = ?e, "lancedb optimize failed; continuing");
    }

    // Bump the churn counter in vectors_meta.json if a meta file is
    // present. Skip when none — that's the "ANN not yet adopted" state,
    // handled by the cold-start path below. Failures here are
    // non-fatal: the next run will recompute against the new state.
    let churn_delta = (deleted + report.chunks_written) as u64;
    if churn_delta > 0
        && let Err(e) = bump_churn(lancedb_dir, churn_delta)
    {
        tracing::warn!(error = ?e, "failed to update vectors_meta churn counter; continuing");
    }

    // Cold-start build / retrain on churn threshold. The L1 optimize
    // above is the pre-step the retrain order requires (compaction
    // first so that tombstones do not flow into the kmeans training
    // set); we do not run it again here.
    if let Err(e) = maybe_build_ann_index(&store, lancedb_dir).await {
        tracing::warn!(error = ?e, "ann build/retrain failed; will retry next run");
    }

    Ok(report)
}

/// Add `delta` to `vectors_meta::churn_since_rebuild` if a meta file is
/// present. Absent meta is a no-op (returns Ok) — the cold-start path
/// owns the first build.
fn bump_churn(lancedb_dir: &Path, delta: u64) -> Result<()> {
    let Some(mut meta) = bookrack_vectors::meta::load(lancedb_dir).map_err(IngestError::Vectors)?
    else {
        return Ok(());
    };
    meta.churn_since_rebuild = meta.churn_since_rebuild.saturating_add(delta);
    bookrack_vectors::meta::store(lancedb_dir, &meta).map_err(IngestError::Vectors)?;
    Ok(())
}

/// L2 retrain threshold: rebuild the ANN index when accumulated churn
/// has reached either twice the size of the corpus at the last build or
/// the absolute floor of 20,000 chunks, whichever is larger. The floor
/// prevents trigger-happy retraining on small libraries.
const L2_ABSOLUTE_FLOOR: u64 = 20_000;

/// Build the ANN index if it should be: either the cold-start case
/// (no meta yet) or the L2 churn-threshold case.
async fn maybe_build_ann_index(store: &ChunkStore, lancedb_dir: &Path) -> Result<()> {
    let template = match bookrack_vectors::meta::load(lancedb_dir).map_err(IngestError::Vectors)? {
        None => {
            // Cold start: no meta file. Build the C1-recommended default.
            AnnConfig::default_for(AnnKind::IvfFlat)
        }
        Some(meta) => {
            // BruteForce kind means a previous drop_ann_index set it
            // explicitly. Respect that — don't auto-rebuild.
            let parsed = AnnConfig::from_meta(&meta).map_err(IngestError::Vectors)?;
            if parsed.kind == AnnKind::BruteForce {
                return Ok(());
            }
            let threshold = (meta.built_at_chunk_count.saturating_mul(2)).max(L2_ABSOLUTE_FLOOR);
            if meta.churn_since_rebuild < threshold {
                return Ok(());
            }
            tracing::info!(
                churn = meta.churn_since_rebuild,
                threshold,
                kind = parsed.kind.as_str(),
                "ann churn threshold crossed; retraining"
            );
            parsed
        }
    };
    let chunk_count = store.count_rows().await.map_err(IngestError::Vectors)? as u64;
    let cfg = scale_to_corpus(template, chunk_count);
    store
        .build_ann_index(&cfg, lancedb_dir, now_rfc3339())
        .await
        .map_err(IngestError::Vectors)?;
    Ok(())
}

/// LanceDB's IVF trainer needs roughly `sample_rate × num_partitions`
/// training vectors (default `sample_rate = 256`). Clamp the requested
/// `num_partitions` so the corpus has enough rows to train them — a
/// brand-new library doing its first small ingest stays at one
/// partition, and grows to the requested value naturally as more
/// books arrive.
fn scale_to_corpus(mut cfg: AnnConfig, chunk_count: u64) -> AnnConfig {
    if cfg.kind == AnnKind::BruteForce {
        return cfg;
    }
    let max_partitions = u32::try_from(chunk_count / 256).unwrap_or(u32::MAX).max(1);
    if cfg.num_partitions > max_partitions {
        cfg.num_partitions = max_partitions;
    }
    cfg
}

/// Current wall-clock time as an RFC 3339 UTC string. Implemented
/// inline to avoid pulling in chrono — uses Howard Hinnant's civil
/// calendar algorithm to turn epoch seconds into a Y/M/D triple.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = (secs % 86_400) as u32;
    let h = day_secs / 3_600;
    let m = (day_secs / 60) % 60;
    let s = day_secs % 60;
    let days = (secs / 86_400) as i64;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert days since 1970-01-01 to a (year, month, day) triple using
/// Howard Hinnant's `days_from_civil` inverse. The math handles
/// arbitrary years without leap-year special cases.
fn days_to_ymd(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Find the end of the greedy batch starting at `start`: grow while the
/// running character count stays under the budget and the chunk count
/// stays under the cap, but always take at least one chunk so an
/// over-budget chunk still makes progress.
fn greedy_batch_end(plans: &[ChunkPlan], start: usize, cfg: &EmbedConfig) -> usize {
    let mut end = start;
    let mut chars = 0usize;
    while end < plans.len() {
        let len = plans[end].text.chars().count();
        if end > start
            && (end - start >= cfg.batch_max_chunks || chars + len > cfg.batch_char_budget)
        {
            break;
        }
        chars += len;
        end += 1;
    }
    end
}

/// Embed one batch, halving and retrying each half on overload until it
/// succeeds or reaches a single chunk. A single chunk cannot shrink
/// further, so an overload there surfaces as an error. `depth` is the
/// recursion depth, recorded on the shrink event so a deep shrink cascade
/// is visible in the logs.
///
/// Returns the embedding vectors and the number of shrink events the call
/// triggered — one per overload that forced a halving, summed across the
/// recursion.
async fn embed_with_shrink<E: Embedder>(
    embedder: &E,
    batch: &[ChunkPlan],
    depth: usize,
) -> Result<(Vec<Vec<f32>>, usize)> {
    let texts: Vec<String> = batch.iter().map(|p| p.text.clone()).collect();
    match embedder.embed_batch(&texts).await {
        Ok(vectors) => Ok((vectors, 0)),
        Err(e) if e.is_overload() && batch.len() > 1 => {
            let mid = batch.len() / 2;
            tracing::warn!(
                before = batch.len(),
                after = mid,
                depth,
                "embed server overloaded; shrinking batch"
            );
            let (mut left, left_shrinks) =
                Box::pin(embed_with_shrink(embedder, &batch[..mid], depth + 1)).await?;
            let (right, right_shrinks) =
                Box::pin(embed_with_shrink(embedder, &batch[mid..], depth + 1)).await?;
            left.extend(right);
            Ok((left, 1 + left_shrinks + right_shrinks))
        }
        Err(e) => Err(IngestError::Embed(e)),
    }
}

/// Zip a batch of plans with its embedding vectors into store rows. The
/// embed client guarantees one vector per input, in order.
fn to_rows(plans: &[ChunkPlan], vectors: Vec<Vec<f32>>) -> Vec<ChunkRow> {
    plans
        .iter()
        .zip(vectors)
        .map(|(p, vector)| ChunkRow {
            vector,
            text: p.text.clone(),
            start_node_id: p.start_node_id,
            start_char_offset: p.start_char_offset,
            end_node_id: p.end_node_id,
            end_char_offset: p.end_char_offset,
            norm_chunk_sha256: p.norm_chunk_sha256.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_core::NodeId;
    use bookrack_corpus::CorpusError;
    use bookrack_embed::{EmbedError, Result as EmbedResult};
    use std::future::Future;

    /// A chunk plan in partition 1 at the given local offset.
    fn plan(offset: i64, text: &str) -> ChunkPlan {
        let node = NodeId::new(100_000_000 + offset);
        ChunkPlan {
            start_node_id: node,
            start_char_offset: 0,
            end_node_id: node,
            end_char_offset: text.chars().count() as i32,
            text: text.to_string(),
            norm_chunk_sha256: bookrack_normalize::norm_text_sha256(text),
        }
    }

    /// A fake embedder returning constant `dim`-length vectors.
    struct Fake {
        dim: usize,
    }

    impl Embedder for Fake {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let dim = self.dim;
            let n = texts.len();
            async move { Ok(vec![vec![0.5f32; dim]; n]) }
        }
    }

    /// A fake that reports overload for any batch larger than `max_batch`,
    /// so the shrink path can be exercised offline.
    struct Overloading {
        dim: usize,
        max_batch: usize,
    }

    impl Embedder for Overloading {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let dim = self.dim;
            let n = texts.len();
            let max = self.max_batch;
            async move {
                if n > max {
                    Err(EmbedError::Overloaded {
                        status: 500,
                        body: String::new(),
                    })
                } else {
                    Ok(vec![vec![0.5f32; dim]; n])
                }
            }
        }
    }

    #[tokio::test]
    async fn embeds_every_chunk_and_writes_one_row_each() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=5).map(|i| plan(i, "some prose text")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let report = embed_book_chunks(
            &plans,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(report.chunks_written, 5);
        // One greedy batch holds all five short chunks, with no shrinking.
        assert_eq!(report.batches, 1);
        assert_eq!(report.shrink_events, 0);

        let store = ChunkStore::open(dir.path(), 8).await.expect("reopen");
        assert_eq!(store.count_rows().await.expect("count"), 5);
    }

    #[test]
    fn scale_to_corpus_clamps_partitions_for_small_libraries() {
        let cfg = bookrack_vectors::AnnConfig::default_for(AnnKind::IvfFlat);
        // Default num_partitions=64; 5 chunks → max 1 partition.
        let scaled = scale_to_corpus(cfg.clone(), 5);
        assert_eq!(scaled.num_partitions, 1);
        // 1024 chunks → max 4 partitions.
        let scaled = scale_to_corpus(cfg.clone(), 1024);
        assert_eq!(scaled.num_partitions, 4);
        // 66_703 chunks → keep the requested 64.
        let scaled = scale_to_corpus(cfg, 66_703);
        assert_eq!(scaled.num_partitions, 64);
    }

    #[test]
    fn scale_to_corpus_leaves_brute_force_alone() {
        let cfg = bookrack_vectors::AnnConfig::default_for(AnnKind::BruteForce);
        assert_eq!(scale_to_corpus(cfg, 0).num_partitions, 0);
    }

    #[tokio::test]
    async fn cold_start_writes_a_scaled_ivf_flat_meta_after_first_book() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=5).map(|i| plan(i, "some text")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_book_chunks(
            &plans,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        assert_eq!(meta.kind, "ivf-flat");
        // 5 chunks is well under sample_rate × num_partitions for the
        // default num_partitions=64, so the cold start clamps to 1.
        assert_eq!(meta.num_partitions, 1);
        assert_eq!(meta.default_nprobes, 40);
        assert_eq!(meta.built_at_chunk_count, 5);
        assert_eq!(meta.churn_since_rebuild, 0);
    }

    #[tokio::test]
    async fn brute_force_meta_is_respected_and_not_auto_rebuilt() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Seed an explicit brute-force meta: a previous `vectors drop`.
        let seed = bookrack_vectors::AnnConfig::default_for(AnnKind::BruteForce).to_meta(
            "2026-06-03T00:00:00Z".to_string(),
            0,
            0,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5).map(|i| plan(i, "some text")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_book_chunks(
            &plans,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        assert_eq!(meta.kind, "brute-force");
        // Churn does grow, but no retrain happens.
        assert_eq!(meta.churn_since_rebuild, 5);
    }

    #[tokio::test]
    async fn churn_below_threshold_does_not_trigger_retrain() {
        let dir = tempfile::tempdir().expect("temp dir");
        let original_ts = "2026-06-03T00:00:00Z";
        // built_at_chunk_count=100 → threshold = max(200, 20000) = 20000.
        // Only 5 churn from one small ingest is well below.
        let seed = bookrack_vectors::AnnConfig::default_for(AnnKind::IvfFlat).to_meta(
            original_ts.to_string(),
            100,
            0,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5).map(|i| plan(i, "some text")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_book_chunks(
            &plans,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        assert_eq!(meta.built_at, original_ts);
        assert_eq!(meta.built_at_chunk_count, 100);
        assert_eq!(meta.churn_since_rebuild, 5);
    }

    #[tokio::test]
    async fn churn_above_threshold_triggers_retrain_and_resets_counter() {
        let dir = tempfile::tempdir().expect("temp dir");
        // built_at_chunk_count=2 → threshold = max(4, 20000) = 20000.
        // Pre-seed churn at 19_999; a 5-chunk ingest pushes it past.
        let seed = bookrack_vectors::AnnConfig::default_for(AnnKind::IvfFlat).to_meta(
            "2026-06-03T00:00:00Z".to_string(),
            2,
            19_999,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5).map(|i| plan(i, "some text")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_book_chunks(
            &plans,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        // Retrain happened: counter reset, built_at_chunk_count updated.
        assert_eq!(meta.churn_since_rebuild, 0);
        assert_eq!(meta.built_at_chunk_count, 5);
        // Partitions scaled down from the seeded 64 to fit corpus.
        assert_eq!(meta.num_partitions, 1);
    }

    #[tokio::test]
    async fn churn_grows_by_inserted_rows_on_first_book_when_meta_exists() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Seed a meta file claiming an index was already built at 0 rows.
        let seed = bookrack_vectors::AnnConfig::default_for(bookrack_vectors::AnnKind::IvfFlat)
            .to_meta(
                "2026-06-03T00:00:00Z".to_string(),
                0,
                0,
                bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
            );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5).map(|i| plan(i, "some text")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_book_chunks(
            &plans,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        // First book: 0 deleted + 5 inserted = 5 churn.
        assert_eq!(meta.churn_since_rebuild, 5);
    }

    #[tokio::test]
    async fn re_embedding_the_same_book_accumulates_delete_and_append_churn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let seed = bookrack_vectors::AnnConfig::default_for(bookrack_vectors::AnnKind::IvfFlat)
            .to_meta(
                "2026-06-03T00:00:00Z".to_string(),
                0,
                0,
                bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
            );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, "text")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        embed_book_chunks(&plans, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first embed");
        embed_book_chunks(&plans, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("second embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        // First run: 0 + 3 = 3. Second run: 3 deleted + 3 inserted = 6.
        // Cumulative: 3 + 6 = 9.
        assert_eq!(meta.churn_since_rebuild, 9);
    }

    #[tokio::test]
    async fn empty_plans_write_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let corpus = Corpus::open_in_memory().expect("corpus");
        let report = embed_book_chunks(
            &[],
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(report, EmbedRunReport::default());
    }

    #[tokio::test]
    async fn re_embedding_replaces_rather_than_duplicates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, "text")).collect();
        let cfg = EmbedConfig::default();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_book_chunks(&plans, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first");
        embed_book_chunks(&plans, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("second");

        let store = ChunkStore::open(dir.path(), 8).await.expect("reopen");
        // The second run deletes the partition before appending, so the
        // count stays at three rather than doubling to six.
        assert_eq!(store.count_rows().await.expect("count"), 3);
    }

    #[tokio::test]
    async fn an_overloaded_batch_shrinks_and_still_writes_every_chunk() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Many small chunks in one greedy batch; the embedder refuses any
        // batch above two, forcing repeated halving down to that size.
        let plans: Vec<ChunkPlan> = (1..=8).map(|i| plan(i, "x")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let report = embed_book_chunks(
            &plans,
            &Overloading {
                dim: 4,
                max_batch: 2,
            },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(report.chunks_written, 8);
        // The eight-chunk batch is halved until each piece is within the
        // server's limit, so the run records the shrinking it took.
        assert!(report.shrink_events > 0);
    }

    #[tokio::test]
    async fn a_run_stamps_the_index_and_then_refuses_a_different_model() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, "prose")).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let first = EmbedConfig {
            model: "model-a".to_string(),
            ..EmbedConfig::default()
        };
        embed_book_chunks(&plans, &Fake { dim: 8 }, &corpus, dir.path(), &first)
            .await
            .expect("first run stamps the index");
        assert_eq!(
            corpus
                .meta_get(bookrack_corpus::EMBED_MODEL_KEY)
                .expect("get"),
            Some("model-a".to_string())
        );

        // A second book embedded with a different model is refused before
        // any vector is written, so the index is never mixed.
        let second = EmbedConfig {
            model: "model-b".to_string(),
            ..EmbedConfig::default()
        };
        let err = embed_book_chunks(&plans, &Fake { dim: 8 }, &corpus, dir.path(), &second)
            .await
            .expect_err("a different model must be refused");
        assert!(matches!(
            err,
            IngestError::Corpus(CorpusError::IndexStampMismatch { key, .. })
                if key == bookrack_corpus::EMBED_MODEL_KEY
        ));
    }
}
