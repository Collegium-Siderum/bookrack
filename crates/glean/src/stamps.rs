// SPDX-License-Identifier: Apache-2.0

//! Paper-side index stamp reconciliation. Peer of
//! `bookrack_ingest::current_index_stamps` for the paper pipeline. The
//! runtime layer composes [`current_index_stamps`] with a `Corpus` open
//! against `papers_corpus.db` to write the stamps after an embedder
//! swap; see `runtime::cmd::papers_stamps`.

use bookrack_corpus::IndexStamps;
use bookrack_normalize::NORMALIZE_VERSION;

use crate::CHUNK_VERSION;

/// Build the [`IndexStamps`] this binary writes into `papers_corpus`'s
/// `index_meta` table. Uses the paper-pipeline's compiled-in
/// [`CHUNK_VERSION`] and the workspace-wide [`NORMALIZE_VERSION`],
/// the embed model name comes from the live runtime configuration, and
/// the vector dimension from a live probe against the embedder.
pub fn current_index_stamps(embed_model: &str, vector_dim: u32) -> IndexStamps {
    IndexStamps {
        embed_model: embed_model.to_string(),
        vector_dim,
        chunk_version: CHUNK_VERSION,
        normalize_version: NORMALIZE_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_index_stamps_carries_paper_chunk_version_and_workspace_normalize_version() {
        let stamps = current_index_stamps("qwen3-embedding:0.6b", 1024);
        assert_eq!(stamps.embed_model, "qwen3-embedding:0.6b");
        assert_eq!(stamps.vector_dim, 1024);
        assert_eq!(stamps.chunk_version, CHUNK_VERSION);
        assert_eq!(stamps.normalize_version, NORMALIZE_VERSION);
    }
}
