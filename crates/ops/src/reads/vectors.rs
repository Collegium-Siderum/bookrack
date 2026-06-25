// SPDX-License-Identifier: Apache-2.0

//! Read ops over the vector store: a snapshot of the chunk table size,
//! every ANN index LanceDB enumerates, and the persisted ANN meta.

use std::collections::HashSet;

use bookrack_corpus::{Corpus, VECTOR_DIM_KEY};
use bookrack_embed::Embedder;
use bookrack_vectors::ChunkStore;

use crate::Ops;
use crate::Result;
use crate::dto::vectors_status::{
    VectorsAnnConfig, VectorsIndexStats, VectorsIndexStatus, VectorsMetaDrift, VectorsMetaSummary,
    VectorsStatus,
};
use crate::recorder::record_call_async;

/// Snapshot the vector store. Read-only end to end: opens the corpus
/// read-only, opens the lancedb directory through `ChunkStore::open`,
/// and walks every enumerated index for its per-shard statistics.
///
/// Returns the "empty" form (every field cleared) when the corpus has
/// no `vector_dim` stamp yet — i.e. no chunks have ever been ingested.
pub async fn status<E: Embedder>(ops: &Ops<E>) -> Result<VectorsStatus> {
    record_call_async!(ops, "library.vectors_status", serde_json::Value::Null, {
        if !ops.corpus_db().exists() {
            return Ok(empty_status());
        }
        let corpus = Corpus::open_read_only(ops.corpus_db())?;
        let Some(dim_str) = corpus.meta_get(VECTOR_DIM_KEY)? else {
            return Ok(empty_status());
        };
        let dim: usize = dim_str.parse().map_err(|e| {
            crate::OpsError::Other(eyre::eyre!("parse vector_dim stamp {dim_str:?}: {e}"))
        })?;
        drop(corpus);

        let lancedb_dir = ops.lancedb_dir();
        let store = ChunkStore::open(lancedb_dir, dim).await?;
        let row_count = store.count_rows().await?;
        let raw_indices = store.list_indices().await?;
        let ann_cfg = store.current_ann_cfg(lancedb_dir)?;
        let meta = bookrack_vectors::meta::load(lancedb_dir)?;

        // LanceDB occasionally enumerates the same index name twice
        // after repeated rebuilds. Keep the first occurrence, drop the
        // duplicates, preserve the order LanceDB returned.
        let mut seen = HashSet::new();
        let unique_indices: Vec<String> = raw_indices
            .into_iter()
            .filter(|n| seen.insert(n.clone()))
            .collect();

        let mut indices = Vec::with_capacity(unique_indices.len());
        for name in &unique_indices {
            let stats = store.index_stats(name).await?.map(|s| VectorsIndexStats {
                index_type: format!("{:?}", s.index_type),
                num_indexed_rows: s.num_indexed_rows,
                num_unindexed_rows: s.num_unindexed_rows,
                num_indices: s.num_indices,
                loss: s.loss,
            });
            indices.push(VectorsIndexStatus {
                name: name.clone(),
                stats,
            });
        }

        let ann_config = ann_cfg.map(|c| VectorsAnnConfig {
            kind: c.kind.as_str().to_string(),
            num_partitions: c.num_partitions,
            nprobes: c.nprobes,
            refine_factor: c.refine_factor,
        });

        let meta_drift = meta.as_ref().and_then(|m| {
            if m.kind != "brute-force" && !unique_indices.contains(&m.lance_index_name) {
                Some(VectorsMetaDrift {
                    expected_index: m.lance_index_name.clone(),
                    found_indices: unique_indices.clone(),
                })
            } else {
                None
            }
        });

        let meta_summary = meta.map(|m| VectorsMetaSummary {
            kind: m.kind,
            lance_index_name: m.lance_index_name,
            churn_since_rebuild: m.churn_since_rebuild,
        });

        Ok(VectorsStatus {
            row_count: Some(row_count),
            indices,
            ann_config,
            meta: meta_summary,
            meta_drift,
        })
    })
}

fn empty_status() -> VectorsStatus {
    VectorsStatus {
        row_count: None,
        indices: Vec::new(),
        ann_config: None,
        meta: None,
        meta_drift: None,
    }
}
