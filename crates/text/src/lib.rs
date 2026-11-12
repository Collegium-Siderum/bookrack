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

mod normalize;

pub use normalize::{FROZEN_DEPS_HASH, NORMALIZE_VERSION, norm_text_sha256, normalize};
