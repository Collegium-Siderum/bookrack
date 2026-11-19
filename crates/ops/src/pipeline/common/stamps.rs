// SPDX-License-Identifier: Apache-2.0

//! The index stamps this build writes, for both pipelines.

use bookrack_corpus::IndexStamps;
use bookrack_text::{CHUNK_VERSION, NORMALIZE_VERSION};

/// The index stamps this binary builds an index with.
///
/// The single assembly point for "what this build expects": the model
/// and vector width are runtime values the caller supplies, while the
/// chunk and normalize versions are this binary's compiled-in constants.
/// The build-side gate in the embed stage and the serve-side gate in the
/// query facade both compare against the [`IndexStamps`] this returns,
/// for a book store and a papers store alike.
pub fn current_index_stamps(embed_model: impl Into<String>, vector_dim: u32) -> IndexStamps {
    IndexStamps {
        embed_model: embed_model.into(),
        vector_dim,
        chunk_version: CHUNK_VERSION,
        normalize_version: NORMALIZE_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_index_stamps_carries_the_pinned_chunk_and_normalize_versions() {
        let stamps = current_index_stamps("qwen3-embedding:0.6b", 1024);
        assert_eq!(stamps.embed_model, "qwen3-embedding:0.6b");
        assert_eq!(stamps.vector_dim, 1024);
        // Literal values, not the constants the function copies: the
        // stamps drive index invalidation, so a version bump must fail
        // here and be acknowledged by updating the pinned numbers.
        assert_eq!(stamps.chunk_version, 1);
        assert_eq!(stamps.normalize_version, 1);
    }
}
