// SPDX-License-Identifier: Apache-2.0

//! Static, offline model registry. Validation consults these tables
//! instead of querying Ollama, because a network round-trip at validate
//! time would break the project's offline guarantee. A model the tables
//! do not list is rejected unless the caller opts into
//! `--allow-unknown-model`.

/// One embedding model's fixed properties: the vector dimension it emits
/// and the model family it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedModelInfo {
    /// The model tag as passed to Ollama.
    pub tag: &'static str,
    /// The embedding dimension the model emits.
    pub dim: u32,
    /// The model family, used to flag cross-family embed/reranker pairs.
    pub family: &'static str,
}

/// One reranker model's fixed properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RerankerModelInfo {
    /// The reranker model identifier.
    pub tag: &'static str,
    /// The model family, matched against the embed family.
    pub family: &'static str,
}

/// Known embedding models. Extend this table when a new embed backend is
/// qualified; the dimension must match what the model actually emits, or
/// a built index will fail its stamp check at serve time.
pub const EMBED_MODELS: &[EmbedModelInfo] = &[
    EmbedModelInfo {
        tag: "qwen3-embedding:0.6b",
        dim: 1024,
        family: "qwen3",
    },
    EmbedModelInfo {
        tag: "qwen3-embedding:4b",
        dim: 2560,
        family: "qwen3",
    },
];

/// Known reranker models.
pub const RERANKER_MODELS: &[RerankerModelInfo] = &[RerankerModelInfo {
    tag: "Qwen3-Reranker-0.6B",
    family: "qwen3",
}];

/// Look up an embedding model by tag.
pub fn embed_model(tag: &str) -> Option<EmbedModelInfo> {
    EMBED_MODELS.iter().copied().find(|m| m.tag == tag)
}

/// Look up a reranker model by tag.
pub fn reranker_model(tag: &str) -> Option<RerankerModelInfo> {
    RERANKER_MODELS.iter().copied().find(|m| m.tag == tag)
}
