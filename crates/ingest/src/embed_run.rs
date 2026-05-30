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
use std::time::Instant;

use bookrack_config::EmbedConfig;
use bookrack_embed::Embedder;
use bookrack_vectors::{ChunkRow, ChunkStore};

use crate::chunk::ChunkPlan;
use crate::{IngestError, Result};

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

    let store = ChunkStore::open(lancedb_dir, dim).await?;
    store
        .delete_partition(plans[0].start_node_id.partition())
        .await?;

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
    Ok(report)
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
        let report = embed_book_chunks(
            &plans,
            &Fake { dim: 8 },
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

    #[tokio::test]
    async fn empty_plans_write_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let report = embed_book_chunks(&[], &Fake { dim: 8 }, dir.path(), &EmbedConfig::default())
            .await
            .expect("embed");
        assert_eq!(report, EmbedRunReport::default());
    }

    #[tokio::test]
    async fn re_embedding_replaces_rather_than_duplicates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, "text")).collect();
        let cfg = EmbedConfig::default();
        embed_book_chunks(&plans, &Fake { dim: 8 }, dir.path(), &cfg)
            .await
            .expect("first");
        embed_book_chunks(&plans, &Fake { dim: 8 }, dir.path(), &cfg)
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
        let report = embed_book_chunks(
            &plans,
            &Overloading {
                dim: 4,
                max_batch: 2,
            },
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
}
