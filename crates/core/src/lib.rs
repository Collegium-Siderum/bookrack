// SPDX-License-Identifier: Apache-2.0

//! Shared domain types and invariant constants.
//!
//! `core` is the foundation of the workspace: the types and constants
//! that cross crate boundaries and the invariants they encode. Every
//! other crate may depend on `core`; `core` itself pulls in only the
//! value-type helpers needed to express its public surface (`serde`,
//! `chrono`).

mod node_type;
mod partition;
pub mod queue;
mod scope;

pub use node_type::NodeType;
pub use partition::{NODE_CAPACITY, NODE_PARTITION_FACTOR, NodeId, PartitionIdx};
pub use scope::{Scope, ScopeParseError};
