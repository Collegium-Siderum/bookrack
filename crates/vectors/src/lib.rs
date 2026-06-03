// SPDX-License-Identifier: Apache-2.0

//! `vectors`: the LanceDB `chunks` table — the dense vector store.
//!
//! This crate owns the read and write side of the slim chunks table:
//! a flat seven-column schema holding one row per embedded chunk. It
//! is the persistence half of the EMBED stage — the `embed` crate
//! produces vectors, this crate stores and searches them.
//!
//! The chunks table carries *no* book or chapter metadata: those are
//! soft-referenced by `start_node_id` and joined from `catalog.db` at
//! query time, so editing metadata never invalidates a vector. A row's
//! owning book is recovered from `start_node_id` by integer division
//! alone (invariant I2), which is also how [`ChunkStore::delete_partition`]
//! erases exactly one book.
//!
//! Scope: this is the store. The IVF-PQ approximate index and the
//! jieba full-text index are deliberate follow-ons — at pilot scale a
//! brute-force [`ChunkStore::search`] is both exact and fast, and the
//! index is only worth building past tens of thousands of rows.

use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Int32Type, Int64Type};
use arrow_array::{
    FixedSizeListArray, Float32Array, Int32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::DistanceType;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::table::OptimizeAction;

use bookrack_core::{NODE_CAPACITY, NODE_PARTITION_FACTOR, NodeId, PartitionIdx};

/// Name of the single table this crate manages.
const TABLE: &str = "chunks";

/// Why a `vectors` operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VectorsError {
    /// The underlying LanceDB layer reported an error.
    #[error("LanceDB error: {0}")]
    Lance(#[from] lancedb::Error),

    /// An Arrow record batch could not be built or decoded.
    #[error("Arrow error: {0}")]
    Arrow(#[from] ArrowError),

    /// A chunk's vector length does not match the dimension the store
    /// was opened with. Every row in the table must share one
    /// dimension, fixed by the embedding model.
    #[error("chunk vector has dimension {got}, store expects {expected}")]
    DimensionMismatch {
        /// The offending vector's length.
        got: usize,
        /// The dimension the store was opened with.
        expected: usize,
    },

    /// A query result was missing an expected column, or it held an
    /// unexpected type — a schema the store did not write.
    #[error("chunks query result is missing or mistyped the {0:?} column")]
    BadColumn(&'static str),
}

/// A fallible `vectors` operation.
pub type Result<T> = std::result::Result<T, VectorsError>;

/// One chunk row about to be written to the store.
///
/// `start_node_id` / `end_node_id` are soft references into
/// `corpus.db`: a chunk spans from a position in one prose leaf to a
/// position in another (the same leaf for a single-paragraph chunk).
#[derive(Debug, Clone)]
pub struct ChunkRow {
    /// The embedding vector; its length must equal the store dimension.
    pub vector: Vec<f32>,
    /// The chunk's verbatim text — what a search result displays.
    pub text: String,
    /// The leaf the chunk starts in.
    pub start_node_id: NodeId,
    /// Character offset of the chunk start within `start_node_id`.
    pub start_char_offset: i32,
    /// The leaf the chunk ends in; equal to `start_node_id` for a
    /// single-paragraph chunk.
    pub end_node_id: NodeId,
    /// Character offset of the chunk end within `end_node_id`.
    pub end_char_offset: i32,
    /// SHA-256 of the normalized chunk text — the query-time
    /// near-duplicate fold key and the embed-cache key.
    pub norm_chunk_sha256: String,
}

/// One hit from a [`ChunkStore::search`], carrying the slim row plus
/// its distance to the query. The vector itself is not returned — it is
/// heavy and the search side never needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The chunk's verbatim text.
    pub text: String,
    /// The leaf the chunk starts in.
    pub start_node_id: NodeId,
    /// Character offset of the chunk start within `start_node_id`.
    pub start_char_offset: i32,
    /// The leaf the chunk ends in.
    pub end_node_id: NodeId,
    /// Character offset of the chunk end within `end_node_id`.
    pub end_char_offset: i32,
    /// SHA-256 of the normalized chunk text.
    pub norm_chunk_sha256: String,
    /// Distance to the query vector under the cosine metric — smaller
    /// is nearer.
    pub distance: f32,
}

/// A handle to the LanceDB `chunks` table.
///
/// All reads and writes of the dense store go through this type;
/// callers never assemble LanceDB queries themselves.
pub struct ChunkStore {
    table: lancedb::Table,
    dim: usize,
}

impl ChunkStore {
    /// Open the chunks table under `lancedb_dir`, creating an empty one
    /// if none exists.
    ///
    /// `dim` is the embedding vector dimension. It is fixed when the
    /// table is first created; reopening a store with a different `dim`
    /// than its rows were written with will fail later, on write or
    /// read — a directory must be reused only with one embedding model.
    pub async fn open(lancedb_dir: &Path, dim: usize) -> Result<ChunkStore> {
        let conn = lancedb::connect(&lancedb_dir.to_string_lossy())
            .execute()
            .await?;
        let names = conn.table_names().execute().await?;
        let table = if names.iter().any(|name| name == TABLE) {
            conn.open_table(TABLE).execute().await?
        } else {
            conn.create_empty_table(TABLE, chunk_schema(dim))
                .execute()
                .await?
        };
        Ok(ChunkStore { table, dim })
    }

    /// The embedding dimension this store holds.
    pub fn dimension(&self) -> usize {
        self.dim
    }

    /// Append chunk rows to the table in one batch. Returns the number
    /// written; an empty input is a no-op. Every vector must match the
    /// store dimension, or the whole batch is rejected before any write.
    pub async fn append(&self, rows: &[ChunkRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        for row in rows {
            if row.vector.len() != self.dim {
                return Err(VectorsError::DimensionMismatch {
                    got: row.vector.len(),
                    expected: self.dim,
                });
            }
        }
        let batch = build_batch(rows, self.dim)?;
        let schema = batch.schema();
        let batch: std::result::Result<RecordBatch, ArrowError> = Ok(batch);
        let reader = RecordBatchIterator::new(std::iter::once(batch), schema);
        self.table
            .add(Box::new(reader) as Box<dyn RecordBatchReader + Send>)
            .execute()
            .await?;
        Ok(rows.len())
    }

    /// Delete every chunk row of one book. The book is named by its
    /// partition; the deletion is a `start_node_id` range filter, the
    /// same form the search prefilter uses. Re-embedding a book is
    /// therefore delete-then-append, which cannot duplicate rows.
    pub async fn delete_partition(&self, partition: PartitionIdx) -> Result<()> {
        let lo = partition.root().get();
        let hi = partition.get() * NODE_PARTITION_FACTOR + NODE_CAPACITY;
        self.table
            .delete(&format!("start_node_id BETWEEN {lo} AND {hi}"))
            .await?;
        Ok(())
    }

    /// Total number of chunk rows in the table.
    pub async fn count_rows(&self) -> Result<usize> {
        Ok(self.table.count_rows(None).await?)
    }

    /// Run table-level maintenance: compact small fragments, prune
    /// versions older than the LanceDB default retention, and absorb any
    /// freshly-appended rows into existing indices.
    ///
    /// Safe to call on an empty table or one with no vector index — the
    /// corresponding step is a no-op in those cases. A book write is
    /// `delete_partition` followed by `append`, which leaves tombstones
    /// and unindexed rows behind; running this at the end of each book
    /// keeps that churn from accumulating.
    pub async fn optimize(&self) -> Result<()> {
        self.table.optimize(OptimizeAction::All).await?;
        Ok(())
    }

    /// Return the `top_k` chunks nearest `query` under cosine distance,
    /// nearest first.
    ///
    /// This is a brute-force scan: exact, and fast enough at pilot
    /// scale. An IVF-PQ index is the follow-on for larger tables.
    pub async fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<SearchHit>> {
        let batches: Vec<RecordBatch> = self
            .table
            .vector_search(query)?
            .distance_type(DistanceType::Cosine)
            .limit(top_k)
            .execute()
            .await?
            .try_collect()
            .await?;
        let mut hits = Vec::new();
        for batch in &batches {
            read_hits(batch, &mut hits)?;
        }
        Ok(hits)
    }
}

/// The slim seven-column chunks schema, parameterized by vector
/// dimension. `vector` is a fixed-size list so LanceDB can index it;
/// every other column is a flat scalar.
fn chunk_schema(dim: usize) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            true,
        ),
        Field::new("text", DataType::Utf8, false),
        Field::new("start_node_id", DataType::Int64, false),
        Field::new("start_char_offset", DataType::Int32, false),
        Field::new("end_node_id", DataType::Int64, false),
        Field::new("end_char_offset", DataType::Int32, false),
        Field::new("norm_chunk_sha256", DataType::Utf8, false),
    ]))
}

/// Build one Arrow record batch from a non-empty slice of chunk rows.
fn build_batch(rows: &[ChunkRow], dim: usize) -> Result<RecordBatch> {
    let vectors = rows
        .iter()
        .map(|r| {
            Some(
                r.vector
                    .iter()
                    .map(|&f| Some(f))
                    .collect::<Vec<Option<f32>>>(),
            )
        })
        .collect::<Vec<_>>();
    let vector_arr =
        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(vectors, dim as i32);

    let text = StringArray::from(rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>());
    let start_node = Int64Array::from(
        rows.iter()
            .map(|r| r.start_node_id.get())
            .collect::<Vec<_>>(),
    );
    let start_off = Int32Array::from(rows.iter().map(|r| r.start_char_offset).collect::<Vec<_>>());
    let end_node = Int64Array::from(rows.iter().map(|r| r.end_node_id.get()).collect::<Vec<_>>());
    let end_off = Int32Array::from(rows.iter().map(|r| r.end_char_offset).collect::<Vec<_>>());
    let sha = StringArray::from(
        rows.iter()
            .map(|r| r.norm_chunk_sha256.clone())
            .collect::<Vec<_>>(),
    );

    let batch = RecordBatch::try_new(
        chunk_schema(dim),
        vec![
            Arc::new(vector_arr),
            Arc::new(text),
            Arc::new(start_node),
            Arc::new(start_off),
            Arc::new(end_node),
            Arc::new(end_off),
            Arc::new(sha),
        ],
    )?;
    Ok(batch)
}

/// Read every row of a search-result batch into `out`.
fn read_hits(batch: &RecordBatch, out: &mut Vec<SearchHit>) -> Result<()> {
    let text = string_column(batch, "text")?;
    let start_node = i64_column(batch, "start_node_id")?;
    let start_off = i32_column(batch, "start_char_offset")?;
    let end_node = i64_column(batch, "end_node_id")?;
    let end_off = i32_column(batch, "end_char_offset")?;
    let sha = string_column(batch, "norm_chunk_sha256")?;
    // `_distance` is the column LanceDB appends to a vector search.
    let distance = f32_column(batch, "_distance")?;

    for i in 0..batch.num_rows() {
        out.push(SearchHit {
            text: text.value(i).to_string(),
            start_node_id: NodeId::new(start_node.value(i)),
            start_char_offset: start_off.value(i),
            end_node_id: NodeId::new(end_node.value(i)),
            end_char_offset: end_off.value(i),
            norm_chunk_sha256: sha.value(i).to_string(),
            distance: distance.value(i),
        });
    }
    Ok(())
}

fn string_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_string_opt::<i32>())
        .ok_or(VectorsError::BadColumn(name))
}

fn i64_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a Int64Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_primitive_opt::<Int64Type>())
        .ok_or(VectorsError::BadColumn(name))
}

fn i32_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a Int32Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_primitive_opt::<Int32Type>())
        .ok_or(VectorsError::BadColumn(name))
}

fn f32_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a Float32Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_primitive_opt::<Float32Type>())
        .ok_or(VectorsError::BadColumn(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DIM: usize = 4;

    /// A chunk row in `partition`, at local offset `offset`, with the
    /// given vector. Text and hash are derived so rows are
    /// distinguishable in assertions.
    fn row(partition: i64, offset: i64, vector: [f32; DIM]) -> ChunkRow {
        let node = PartitionIdx::new(partition)
            .node_id(offset)
            .expect("offset is in range");
        ChunkRow {
            vector: vector.to_vec(),
            text: format!("chunk p{partition} o{offset}"),
            start_node_id: node,
            start_char_offset: 0,
            end_node_id: node,
            end_char_offset: 100,
            norm_chunk_sha256: format!("sha-p{partition}-o{offset}"),
        }
    }

    /// Open a fresh store in a temp directory. The `TempDir` is returned
    /// so the test keeps it alive (drop cleans the directory up).
    async fn fresh_store() -> (TempDir, ChunkStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ChunkStore::open(dir.path(), DIM).await.expect("open");
        (dir, store)
    }

    #[tokio::test]
    async fn append_then_count_round_trips() {
        let (_dir, store) = fresh_store().await;
        let written = store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(1, 3, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        assert_eq!(written, 3);
        assert_eq!(store.count_rows().await.expect("count"), 3);
    }

    #[tokio::test]
    async fn an_empty_append_writes_nothing() {
        let (_dir, store) = fresh_store().await;
        assert_eq!(store.append(&[]).await.expect("append"), 0);
        assert_eq!(store.count_rows().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn a_wrong_dimension_vector_is_rejected() {
        let (_dir, store) = fresh_store().await;
        let mut bad = row(1, 1, [1.0, 0.0, 0.0, 0.0]);
        bad.vector = vec![1.0, 0.0, 0.0]; // three elements, store expects four
        let err = store.append(&[bad]).await.unwrap_err();
        assert!(
            matches!(
                err,
                VectorsError::DimensionMismatch {
                    got: 3,
                    expected: 4
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_partition_erases_only_that_book() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(2, 1, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        store
            .delete_partition(PartitionIdx::new(1))
            .await
            .expect("delete");
        // Only partition 2's single row survives.
        assert_eq!(store.count_rows().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn search_returns_the_nearest_chunk_first() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(1, 3, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        // A query closest to the first row's direction.
        let hits = store
            .search(&[0.9, 0.1, 0.0, 0.0], 3)
            .await
            .expect("search");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].text, "chunk p1 o1");
        assert_eq!(hits[0].start_node_id, NodeId::new(100_000_001));
        // Distances are sorted nearest-first.
        assert!(hits[0].distance <= hits[1].distance);
    }

    #[tokio::test]
    async fn optimize_on_an_empty_table_is_a_noop() {
        let (_dir, store) = fresh_store().await;
        store.optimize().await.expect("optimize");
        assert_eq!(store.count_rows().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn optimize_after_append_keeps_rows_and_can_search() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
            ])
            .await
            .expect("append");
        store.optimize().await.expect("optimize");
        assert_eq!(store.count_rows().await.expect("count"), 2);
        let hits = store
            .search(&[0.9, 0.1, 0.0, 0.0], 2)
            .await
            .expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "chunk p1 o1");
    }

    #[tokio::test]
    async fn optimize_after_delete_then_append_clears_tombstones() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(1, 3, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        // Simulate the book-write pattern: delete the partition, append
        // fresh rows, then optimize. The optimize call must succeed and
        // the row count must reflect only the second batch.
        store
            .delete_partition(PartitionIdx::new(1))
            .await
            .expect("delete");
        store
            .append(&[
                row(1, 1, [0.5, 0.5, 0.0, 0.0]),
                row(1, 2, [0.5, 0.0, 0.5, 0.0]),
            ])
            .await
            .expect("re-append");
        store.optimize().await.expect("optimize");
        assert_eq!(store.count_rows().await.expect("count"), 2);
    }

    #[tokio::test]
    async fn a_reopened_store_sees_existing_rows() {
        let dir = tempfile::tempdir().expect("temp dir");
        {
            let store = ChunkStore::open(dir.path(), DIM).await.expect("open");
            store
                .append(&[row(1, 1, [1.0, 0.0, 0.0, 0.0])])
                .await
                .expect("append");
        }
        // A second handle on the same directory finds the committed row.
        let reopened = ChunkStore::open(dir.path(), DIM).await.expect("reopen");
        assert_eq!(reopened.count_rows().await.expect("count"), 1);
    }
}
