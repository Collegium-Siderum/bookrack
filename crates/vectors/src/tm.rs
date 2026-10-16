// SPDX-License-Identifier: Apache-2.0

//! The translation-memory table: one row per sealed translation
//! segment, searched by source-text similarity within a language pair.
//!
//! A [`TmStore`] lives in its own LanceDB directory, apart from the
//! `chunks` table, because every tool that manages `chunks` — the
//! `vectors_meta.json` sidecar, the ANN index, reset and reembed —
//! binds to that one table and would neither see nor refresh a second
//! one. The memory is derived from the translation store: every row is
//! recomputable from a sealed segment, so the directory carries no
//! sidecar and no reader stamp of its own. The build stamps that say
//! which embedding model wrote the rows live beside the segments, in
//! `translate_meta`, and are checked by the callers of this store.
//!
//! The table is scanned flat: a library seals thousands of segments,
//! not millions, and an exact scan at that scale costs less than an
//! index is worth.

use std::path::Path;
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{
    FixedSizeListArray, Int32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::DistanceType;
use lancedb::query::{ExecutableQuery, QueryBase};

use crate::{
    Result, VectorsError, connect, ensure_lance_env, f32_column, i32_column, i64_column,
    string_column, vector_dim_from_schema,
};

/// Name of the translation-memory table.
pub const TM_TABLE: &str = "translate_segments_vec";

/// The row layout this build writes, stamped into `translate_meta` by
/// the writer and compared on every read.
///
/// Bump when a column is added, removed or repurposed, so a memory
/// written by another build is rebuilt rather than misread.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const TM_SCHEMA_VERSION: u32 = 1;

/// Longest language tag a filter admits.
// setting: internal -- a data-format invariant of the filter predicate
const MAX_LANG_LEN: usize = 16;

/// One translation-memory row.
#[derive(Debug, Clone, PartialEq)]
pub struct TmRow {
    /// Embedding of `src_text`. Empty on a row read back from a search,
    /// where only the distance is wanted.
    pub vector: Vec<f32>,
    /// The sealed segment this row mirrors.
    pub segment_id: i64,
    /// The segment's unit.
    pub unit_id: i64,
    /// The book the segment belongs to.
    pub intake_id: i64,
    /// Language the segment was translated into.
    pub target_lang: String,
    /// Language of the source text, as the catalog records it for the
    /// book, or `und` when it records none.
    pub source_lang: String,
    /// Start leaf of the segment's span.
    pub start_node_id: i64,
    /// Character offset within the start leaf, inclusive.
    pub start_char_offset: i32,
    /// End leaf of the segment's span.
    pub end_node_id: i64,
    /// Character offset within the end leaf, exclusive.
    pub end_char_offset: i32,
    /// The source text the vector embeds.
    pub src_text: String,
    /// The sealed final text.
    pub dst_text: String,
    /// Where the final text came from, one of the segment store's
    /// `source_kind` values.
    pub source_kind: String,
    /// When the segment was sealed, RFC 3339.
    pub sealed_at: String,
}

/// What a search is confined to: a language pair, and optionally one
/// book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TmFilter<'a> {
    pub target_lang: &'a str,
    pub source_lang: &'a str,
    pub intake_id: Option<i64>,
}

impl TmFilter<'_> {
    /// The LanceDB predicate for this filter, or `BadFilter` when a
    /// language tag is not `[A-Za-z0-9-]{1,16}`.
    ///
    /// The two tags are the only caller-supplied strings that reach a
    /// predicate, and the whitelist is what keeps them data rather than
    /// syntax; the book id is an integer and needs no quoting.
    pub fn predicate(&self) -> Result<String> {
        check_lang(self.target_lang, "target_lang")?;
        check_lang(self.source_lang, "source_lang")?;
        let mut predicate = format!(
            "target_lang = '{}' AND source_lang = '{}'",
            self.target_lang, self.source_lang
        );
        if let Some(intake_id) = self.intake_id {
            predicate.push_str(&format!(" AND intake_id = {intake_id}"));
        }
        Ok(predicate)
    }
}

/// True when `tag` is one to sixteen ASCII letters, digits or hyphens.
fn is_lang_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= MAX_LANG_LEN
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn check_lang(tag: &str, field: &'static str) -> Result<()> {
    if is_lang_tag(tag) {
        Ok(())
    } else {
        Err(VectorsError::BadFilter(field))
    }
}

/// One search result: the row, without its vector, and its cosine
/// distance to the query — smaller is nearer.
#[derive(Debug, Clone, PartialEq)]
pub struct TmHitRow {
    pub row: TmRow,
    pub distance: f32,
}

/// A handle to the translation-memory table.
///
/// Every read and write of the memory goes through this type; callers
/// never assemble LanceDB queries themselves.
pub struct TmStore {
    table: lancedb::Table,
    dim: usize,
}

impl TmStore {
    /// Open the existing memory under `lancedb_dir`, or `None` when
    /// the directory or the table is absent. Never creates either.
    pub async fn try_open(lancedb_dir: &Path) -> Result<Option<TmStore>> {
        ensure_lance_env();
        if !lancedb_dir.exists() {
            return Ok(None);
        }
        let conn = connect(lancedb_dir).await?;
        let names = conn.table_names().execute().await?;
        if !names.iter().any(|name| name == TM_TABLE) {
            return Ok(None);
        }
        let table = conn.open_table(TM_TABLE).execute().await?;
        let dim = vector_dim_from_schema(table.schema().await?.as_ref())?;
        Ok(Some(TmStore { table, dim }))
    }

    /// Open the memory under `lancedb_dir`, creating an empty table of
    /// `dim`-wide vectors when there is none. An existing table keeps
    /// the dimension it was created with; `dim` is a creation hint.
    pub async fn open(lancedb_dir: &Path, dim: usize) -> Result<TmStore> {
        ensure_lance_env();
        std::fs::create_dir_all(lancedb_dir)?;
        let conn = connect(lancedb_dir).await?;
        let names = conn.table_names().execute().await?;
        let (table, dim) = if names.iter().any(|name| name == TM_TABLE) {
            let table = conn.open_table(TM_TABLE).execute().await?;
            let on_disk_dim = vector_dim_from_schema(table.schema().await?.as_ref())?;
            (table, on_disk_dim)
        } else {
            (
                conn.create_empty_table(TM_TABLE, tm_schema(dim))
                    .execute()
                    .await?,
                dim,
            )
        };
        Ok(TmStore { table, dim })
    }

    /// Drop the memory table under `lancedb_dir`. Idempotent: a missing
    /// directory or table is accepted. Touches nothing else in the
    /// directory.
    pub async fn drop_table(lancedb_dir: &Path) -> Result<()> {
        ensure_lance_env();
        if !lancedb_dir.exists() {
            return Ok(());
        }
        let conn = connect(lancedb_dir).await?;
        let names = conn.table_names().execute().await?;
        if names.iter().any(|name| name == TM_TABLE) {
            conn.drop_table(TM_TABLE, &[]).await?;
        }
        Ok(())
    }

    /// The vector dimension this memory holds.
    pub fn dimension(&self) -> usize {
        self.dim
    }

    /// Append rows in one batch. Returns the number written; an empty
    /// input is a no-op. Every vector must match the store dimension,
    /// or the whole batch is rejected before any write.
    pub async fn append(&self, rows: &[TmRow]) -> Result<usize> {
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
        let batch = build_tm_batch(rows, self.dim)?;
        let schema = batch.schema();
        let batch: std::result::Result<RecordBatch, ArrowError> = Ok(batch);
        let reader = RecordBatchIterator::new(std::iter::once(batch), schema);
        self.table
            .add(Box::new(reader) as Box<dyn RecordBatchReader + Send>)
            .execute()
            .await?;
        Ok(rows.len())
    }

    /// Delete the rows of the named segments. An empty list is a no-op.
    pub async fn delete_segments(&self, segment_ids: &[i64]) -> Result<()> {
        if segment_ids.is_empty() {
            return Ok(());
        }
        let list = segment_ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        self.table
            .delete(&format!("segment_id IN ({list})"))
            .await?;
        Ok(())
    }

    /// Delete every row of one book.
    pub async fn delete_intake(&self, intake_id: i64) -> Result<()> {
        self.table
            .delete(&format!("intake_id = {intake_id}"))
            .await?;
        Ok(())
    }

    /// Total number of rows in the memory.
    pub async fn count_rows(&self) -> Result<usize> {
        Ok(self.table.count_rows(None).await?)
    }

    /// The `top_k` rows nearest `query` under cosine distance within
    /// `filter`, nearest first. Vectors are not read back.
    pub async fn search(
        &self,
        query: &[f32],
        filter: &TmFilter<'_>,
        top_k: usize,
    ) -> Result<Vec<TmHitRow>> {
        let predicate = filter.predicate()?;
        let batches: Vec<RecordBatch> = self
            .table
            .vector_search(query)?
            .distance_type(DistanceType::Cosine)
            .only_if(predicate)
            .limit(top_k)
            .execute()
            .await?
            .try_collect()
            .await?;
        let mut hits = Vec::new();
        for batch in &batches {
            read_tm_hits(batch, &mut hits)?;
        }
        Ok(hits)
    }
}

/// The memory's schema, parameterized by vector dimension. `vector` is
/// a fixed-size list so LanceDB can search it; every other column is a
/// flat, non-null scalar.
fn tm_schema(dim: usize) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            true,
        ),
        Field::new("segment_id", DataType::Int64, false),
        Field::new("unit_id", DataType::Int64, false),
        Field::new("intake_id", DataType::Int64, false),
        Field::new("target_lang", DataType::Utf8, false),
        Field::new("source_lang", DataType::Utf8, false),
        Field::new("start_node_id", DataType::Int64, false),
        Field::new("start_char_offset", DataType::Int32, false),
        Field::new("end_node_id", DataType::Int64, false),
        Field::new("end_char_offset", DataType::Int32, false),
        Field::new("src_text", DataType::Utf8, false),
        Field::new("dst_text", DataType::Utf8, false),
        Field::new("source_kind", DataType::Utf8, false),
        Field::new("sealed_at", DataType::Utf8, false),
    ]))
}

/// Build one Arrow record batch from a non-empty slice of rows.
fn build_tm_batch(rows: &[TmRow], dim: usize) -> Result<RecordBatch> {
    let vectors = rows
        .iter()
        .map(|r| Some(r.vector.iter().map(|&f| Some(f)).collect::<Vec<_>>()))
        .collect::<Vec<_>>();
    let vector_arr =
        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(vectors, dim as i32);
    let i64s = |f: fn(&TmRow) -> i64| Int64Array::from(rows.iter().map(f).collect::<Vec<_>>());
    let i32s = |f: fn(&TmRow) -> i32| Int32Array::from(rows.iter().map(f).collect::<Vec<_>>());
    let strs = |f: fn(&TmRow) -> &str| StringArray::from_iter_values(rows.iter().map(f));
    let batch = RecordBatch::try_new(
        tm_schema(dim),
        vec![
            Arc::new(vector_arr),
            Arc::new(i64s(|r| r.segment_id)),
            Arc::new(i64s(|r| r.unit_id)),
            Arc::new(i64s(|r| r.intake_id)),
            Arc::new(strs(|r| &r.target_lang)),
            Arc::new(strs(|r| &r.source_lang)),
            Arc::new(i64s(|r| r.start_node_id)),
            Arc::new(i32s(|r| r.start_char_offset)),
            Arc::new(i64s(|r| r.end_node_id)),
            Arc::new(i32s(|r| r.end_char_offset)),
            Arc::new(strs(|r| &r.src_text)),
            Arc::new(strs(|r| &r.dst_text)),
            Arc::new(strs(|r| &r.source_kind)),
            Arc::new(strs(|r| &r.sealed_at)),
        ],
    )?;
    Ok(batch)
}

/// Read every row of a search-result batch into `out`, scalars and
/// `_distance` only.
fn read_tm_hits(batch: &RecordBatch, out: &mut Vec<TmHitRow>) -> Result<()> {
    let segment_id = i64_column(batch, "segment_id")?;
    let unit_id = i64_column(batch, "unit_id")?;
    let intake_id = i64_column(batch, "intake_id")?;
    let target_lang = string_column(batch, "target_lang")?;
    let source_lang = string_column(batch, "source_lang")?;
    let start_node = i64_column(batch, "start_node_id")?;
    let start_off = i32_column(batch, "start_char_offset")?;
    let end_node = i64_column(batch, "end_node_id")?;
    let end_off = i32_column(batch, "end_char_offset")?;
    let src_text = string_column(batch, "src_text")?;
    let dst_text = string_column(batch, "dst_text")?;
    let source_kind = string_column(batch, "source_kind")?;
    let sealed_at = string_column(batch, "sealed_at")?;
    // `_distance` is the column LanceDB appends to a vector search.
    let distance = f32_column(batch, "_distance")?;
    for i in 0..batch.num_rows() {
        out.push(TmHitRow {
            row: TmRow {
                vector: Vec::new(),
                segment_id: segment_id.value(i),
                unit_id: unit_id.value(i),
                intake_id: intake_id.value(i),
                target_lang: target_lang.value(i).to_string(),
                source_lang: source_lang.value(i).to_string(),
                start_node_id: start_node.value(i),
                start_char_offset: start_off.value(i),
                end_node_id: end_node.value(i),
                end_char_offset: end_off.value(i),
                src_text: src_text.value(i).to_string(),
                dst_text: dst_text.value(i).to_string(),
                source_kind: source_kind.value(i).to_string(),
                sealed_at: sealed_at.value(i).to_string(),
            },
            distance: distance.value(i),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DIM: usize = 4;

    fn row(segment_id: i64, intake_id: i64, pair: (&str, &str), vector: [f32; DIM]) -> TmRow {
        TmRow {
            vector: vector.to_vec(),
            segment_id,
            unit_id: intake_id * 10,
            intake_id,
            target_lang: pair.1.to_string(),
            source_lang: pair.0.to_string(),
            start_node_id: 100_000_000 + segment_id,
            start_char_offset: 0,
            end_node_id: 100_000_000 + segment_id,
            end_char_offset: 12,
            src_text: format!("source {segment_id}"),
            dst_text: format!("target {segment_id}"),
            source_kind: "human".to_string(),
            sealed_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    async fn fresh_store() -> (TempDir, TmStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = TmStore::open(dir.path(), DIM).await.expect("open");
        (dir, store)
    }

    const DE_ZH: (&str, &str) = ("de", "zh");
    const EN_ZH: (&str, &str) = ("en", "zh");

    #[tokio::test]
    async fn try_open_on_a_missing_dir_returns_none_without_materializing_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("lancedb_translate");
        let store = TmStore::try_open(&missing).await.expect("try_open");
        assert!(store.is_none());
        assert!(!missing.exists(), "a probe must not create the directory");
    }

    #[tokio::test]
    async fn open_then_try_open_round_trips_the_dimension() {
        let (dir, store) = fresh_store().await;
        assert_eq!(store.dimension(), DIM);
        drop(store);
        let reopened = TmStore::try_open(dir.path())
            .await
            .expect("try_open")
            .expect("table exists");
        assert_eq!(reopened.dimension(), DIM);
        // A different hint does not change the on-disk dimension.
        let hinted = TmStore::open(dir.path(), DIM + 3).await.expect("open");
        assert_eq!(hinted.dimension(), DIM);
    }

    #[tokio::test]
    async fn append_then_search_returns_the_nearest_row_first_within_the_language_pair() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 7, DE_ZH, [1.0, 0.0, 0.0, 0.0]),
                row(2, 7, DE_ZH, [0.0, 1.0, 0.0, 0.0]),
                // The same direction as row 1 and the same target language,
                // but from another source language.
                row(3, 7, EN_ZH, [1.0, 0.0, 0.0, 0.0]),
            ])
            .await
            .expect("append");
        assert_eq!(store.count_rows().await.expect("count"), 3);

        let filter = TmFilter {
            target_lang: "zh",
            source_lang: "de",
            intake_id: None,
        };
        let hits = store
            .search(&[0.9, 0.1, 0.0, 0.0], &filter, 10)
            .await
            .expect("search");
        let ids: Vec<i64> = hits.iter().map(|h| h.row.segment_id).collect();
        assert_eq!(ids, vec![1, 2], "row 3 is another language pair");
        assert!(hits[0].distance <= hits[1].distance);
        assert_eq!(hits[0].row.dst_text, "target 1");
        assert!(hits[0].row.vector.is_empty(), "vectors are not read back");
    }

    #[tokio::test]
    async fn intake_scope_confines_hits_to_one_book() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 7, DE_ZH, [1.0, 0.0, 0.0, 0.0]),
                row(2, 8, DE_ZH, [1.0, 0.0, 0.0, 0.0]),
            ])
            .await
            .expect("append");
        let filter = TmFilter {
            target_lang: "zh",
            source_lang: "de",
            intake_id: Some(8),
        };
        let hits = store
            .search(&[1.0, 0.0, 0.0, 0.0], &filter, 10)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.intake_id, 8);
    }

    #[tokio::test]
    async fn a_language_tag_outside_the_whitelist_is_refused_before_any_query() {
        let (_dir, store) = fresh_store().await;
        for (target, source, field) in [
            ("zh'; DROP TABLE x; --", "de", "target_lang"),
            ("zh", "", "source_lang"),
            ("zh", "de OR 1=1", "source_lang"),
            ("abcdefghijklmnopq", "de", "target_lang"),
        ] {
            let filter = TmFilter {
                target_lang: target,
                source_lang: source,
                intake_id: None,
            };
            let err = store
                .search(&[1.0, 0.0, 0.0, 0.0], &filter, 10)
                .await
                .expect_err("refused");
            assert!(
                matches!(err, VectorsError::BadFilter(f) if f == field),
                "{target:?}/{source:?}: {err:?}"
            );
        }
        let ok = TmFilter {
            target_lang: "zh-Hant",
            source_lang: "de",
            intake_id: Some(3),
        };
        assert_eq!(
            ok.predicate().expect("predicate"),
            "target_lang = 'zh-Hant' AND source_lang = 'de' AND intake_id = 3"
        );
    }

    #[tokio::test]
    async fn delete_segments_and_delete_intake_remove_only_their_rows() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 7, DE_ZH, [1.0, 0.0, 0.0, 0.0]),
                row(2, 7, DE_ZH, [0.0, 1.0, 0.0, 0.0]),
                row(3, 8, DE_ZH, [0.0, 0.0, 1.0, 0.0]),
                row(4, 8, DE_ZH, [0.0, 0.0, 0.0, 1.0]),
            ])
            .await
            .expect("append");
        store.delete_segments(&[]).await.expect("empty is a no-op");
        assert_eq!(store.count_rows().await.expect("count"), 4);
        store.delete_segments(&[1]).await.expect("delete one");
        assert_eq!(store.count_rows().await.expect("count"), 3);
        store.delete_intake(8).await.expect("delete book");
        assert_eq!(store.count_rows().await.expect("count"), 1);
        let filter = TmFilter {
            target_lang: "zh",
            source_lang: "de",
            intake_id: None,
        };
        let hits = store
            .search(&[0.0, 1.0, 0.0, 0.0], &filter, 10)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.segment_id, 2);
    }

    #[tokio::test]
    async fn append_rejects_a_vector_of_another_width_before_writing() {
        let (_dir, store) = fresh_store().await;
        let mut bad = row(1, 7, DE_ZH, [1.0, 0.0, 0.0, 0.0]);
        bad.vector.push(0.5);
        let err = store
            .append(&[row(2, 7, DE_ZH, [0.0, 1.0, 0.0, 0.0]), bad])
            .await
            .expect_err("rejected");
        assert!(matches!(
            err,
            VectorsError::DimensionMismatch {
                got: 5,
                expected: 4
            }
        ));
        assert_eq!(store.count_rows().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn drop_table_is_idempotent_and_leaves_no_sidecar() {
        let (dir, store) = fresh_store().await;
        store
            .append(&[row(1, 7, DE_ZH, [1.0, 0.0, 0.0, 0.0])])
            .await
            .expect("append");
        drop(store);
        TmStore::drop_table(dir.path()).await.expect("drop");
        assert!(
            TmStore::try_open(dir.path())
                .await
                .expect("probe")
                .is_none()
        );
        assert!(!dir.path().join(crate::META_FILENAME).exists());
        TmStore::drop_table(dir.path()).await.expect("drop again");
        TmStore::drop_table(&dir.path().join("never-made"))
            .await
            .expect("drop of a missing dir");
    }
}
