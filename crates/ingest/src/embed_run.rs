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

use bookrack_config::EmbedConfig;
use bookrack_embed::Embedder;
use bookrack_vectors::{ChunkRow, ChunkStore};

use crate::chunk::ChunkPlan;
use crate::{IngestError, Result};

/// Embed a book's chunk plans and write them to the store under
/// `lancedb_dir`. Returns the number of rows written.
///
/// The embedding dimension is probed from the first chunk and the store
/// is opened fixed to it. The book's prior rows are cleared before the
/// first append, keying the deletion on the partition the chunks live in.
pub async fn embed_book_chunks<E: Embedder>(
    plans: &[ChunkPlan],
    embedder: &E,
    lancedb_dir: &Path,
    cfg: &EmbedConfig,
) -> Result<usize> {
    if plans.is_empty() {
        return Ok(0);
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

    let mut written = 0usize;
    let mut start = 0usize;
    while start < plans.len() {
        let end = greedy_batch_end(plans, start, cfg);
        let batch = &plans[start..end];
        let vectors = embed_with_shrink(embedder, batch).await?;
        written += store.append(&to_rows(batch, vectors)).await?;
        start = end;
    }
    Ok(written)
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
/// further, so an overload there surfaces as an error.
async fn embed_with_shrink<E: Embedder>(
    embedder: &E,
    batch: &[ChunkPlan],
) -> Result<Vec<Vec<f32>>> {
    let texts: Vec<String> = batch.iter().map(|p| p.text.clone()).collect();
    match embedder.embed_batch(&texts).await {
        Ok(vectors) => Ok(vectors),
        Err(e) if e.is_overload() && batch.len() > 1 => {
            let mid = batch.len() / 2;
            let mut left = Box::pin(embed_with_shrink(embedder, &batch[..mid])).await?;
            let right = Box::pin(embed_with_shrink(embedder, &batch[mid..])).await?;
            left.extend(right);
            Ok(left)
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
        let written = embed_book_chunks(
            &plans,
            &Fake { dim: 8 },
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(written, 5);

        let store = ChunkStore::open(dir.path(), 8).await.expect("reopen");
        assert_eq!(store.count_rows().await.expect("count"), 5);
    }

    #[tokio::test]
    async fn empty_plans_write_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let written = embed_book_chunks(&[], &Fake { dim: 8 }, dir.path(), &EmbedConfig::default())
            .await
            .expect("embed");
        assert_eq!(written, 0);
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
        let written = embed_book_chunks(
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
        assert_eq!(written, 8);
    }
}
