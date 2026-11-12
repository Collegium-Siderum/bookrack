// SPDX-License-Identifier: Apache-2.0

//! Text primitives shared across the workspace.
//!
//! This crate owns the operations whose exact output is part of a
//! stored identity, and it is the only crate that depends directly on
//! a segmentation or normalization library. Each module holds one
//! concept; every public item is re-exported here, so callers name
//! `bookrack_text::<item>`.
//!
//! - `normalize`: the frozen canonical form of prose and its content
//!   hash, versioned by [`NORMALIZE_VERSION`].
//! - `chunk`: grouping prose leaves and splitting them into the
//!   retrieval units that get embedded, versioned by [`CHUNK_VERSION`].
//! - `sentence`: the sentence terminators, the per-leaf sentence count
//!   versioned by [`SENTENCE_VERSION`], and sentence boundaries.
//! - `script`: predicates over CJK scripts and the fullwidth bracket
//!   pairs, each set frozen under its name.

mod chunk;
mod normalize;
mod script;
mod sentence;

pub use chunk::{
    CHUNK_VERSION, ChunkLeaf, ChunkParams, ChunkPlan, FROZEN_SPLITTER_DEPS, plan_chunks,
};
pub use normalize::{FROZEN_DEPS_HASH, NORMALIZE_VERSION, norm_text_sha256, normalize};
pub use script::{
    FULLWIDTH_PARENS, FULLWIDTH_SQUARE_BRACKETS, LENTICULAR_BRACKETS, is_cjk_script, is_han,
    is_hangul_syllable, is_kana,
};
pub use sentence::{
    SENTENCE_VERSION, count_sentences, is_closer, is_terminator, is_unambiguous_terminator,
    sentence_boundaries, split_at_sentences,
};
