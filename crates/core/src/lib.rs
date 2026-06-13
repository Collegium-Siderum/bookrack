// SPDX-License-Identifier: Apache-2.0

//! Shared domain types and invariant constants.
//!
//! `core` is the foundation of the workspace: the types and constants
//! that cross crate boundaries and the invariants they encode. Every
//! other crate may depend on `core`; `core` itself pulls in only the
//! value-type helpers needed to express its public surface (`serde`,
//! `chrono`).

mod error_chain;
mod item_kind;
mod kinded_node_id;
mod node_type;
mod partition;
pub mod queue;
mod scope;

pub use error_chain::error_chain;
pub use item_kind::ItemKind;
pub use kinded_node_id::KindedNodeId;
pub use node_type::NodeType;
pub use partition::{NODE_CAPACITY, NODE_PARTITION_FACTOR, NodeId, PartitionIdx};
pub use scope::{Scope, ScopeParseError};
