// SPDX-License-Identifier: Apache-2.0

//! ingest: assemble an `extract::Extraction` into the persistent data
//! model.
//!
//! This module is the first building block of that assembly: a frozen,
//! deterministic sentence counter that the STRUCTURE stage uses to fill
//! each prose leaf's statistics. The tree-building, chunking and
//! embedding stages build on top of it.

pub mod sentences;
