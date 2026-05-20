// SPDX-License-Identifier: Apache-2.0

//! Shared domain types and invariant constants.
//!
//! `core` is the dependency-free foundation of the workspace: the
//! types and constants that cross crate boundaries and the invariants
//! they encode. Every other crate may depend on `core`; `core` itself
//! depends on nothing.

mod node_type;
mod partition;

pub use node_type::NodeType;
pub use partition::{NODE_CAPACITY, NODE_PARTITION_FACTOR, NodeId, PartitionIdx};
