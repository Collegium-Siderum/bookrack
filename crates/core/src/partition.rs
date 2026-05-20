// SPDX-License-Identifier: Apache-2.0

//! The node-id partition protocol (invariant I2).
//!
//! Every node in the corpus has a single global `i64` id. The id space
//! is partitioned by book: one book (one ingested file) owns a
//! contiguous block of [`NODE_PARTITION_FACTOR`] ids, and
//!
//! ```text
//! node_id = partition_idx * NODE_PARTITION_FACTOR + local_offset
//! ```
//!
//! so the owning book of any node is recovered by integer division
//! alone — no table lookup or join. `local_offset` runs from 1 to
//! [`NODE_CAPACITY`]; offset 1 is reserved for the book's root node.

use std::fmt;

/// Size of each book's id block. A node id divided by this factor is
/// its partition index; the remainder is its local offset.
///
/// Frozen invariant I2 — changing it renumbers every node id.
pub const NODE_PARTITION_FACTOR: i64 = 100_000_000;

/// Largest valid `local_offset`: the maximum number of nodes one book
/// can hold. Offsets run `1..=NODE_CAPACITY`; offset 0 would alias the
/// partition boundary and is never a real node.
pub const NODE_CAPACITY: i64 = NODE_PARTITION_FACTOR - 1;

/// A node's global identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(i64);

impl NodeId {
    /// Wrap a raw id, e.g. one read from the database.
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    /// The raw id, e.g. for writing back to the database.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// The book partition this node belongs to.
    pub const fn partition(self) -> PartitionIdx {
        PartitionIdx(self.0 / NODE_PARTITION_FACTOR)
    }

    /// This node's offset within its partition.
    pub const fn local_offset(self) -> i64 {
        self.0 % NODE_PARTITION_FACTOR
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The index of a book's id partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionIdx(i64);

impl PartitionIdx {
    /// Wrap a raw partition index.
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    /// The raw partition index.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// This partition's root node — local offset 1.
    pub const fn root(self) -> NodeId {
        NodeId(self.0 * NODE_PARTITION_FACTOR + 1)
    }

    /// Compose a node id from a local offset, or `None` if the offset
    /// is outside `1..=NODE_CAPACITY` and would escape the partition.
    pub fn node_id(self, local_offset: i64) -> Option<NodeId> {
        if (1..=NODE_CAPACITY).contains(&local_offset) {
            Some(NodeId(self.0 * NODE_PARTITION_FACTOR + local_offset))
        } else {
            None
        }
    }

    /// Whether `node` belongs to this partition.
    pub const fn contains(self, node: NodeId) -> bool {
        node.0 / NODE_PARTITION_FACTOR == self.0
    }
}

impl fmt::Display for PartitionIdx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_and_capacity() {
        assert_eq!(NODE_PARTITION_FACTOR, 100_000_000);
        assert_eq!(NODE_CAPACITY, 99_999_999);
    }

    #[test]
    fn id_splits_into_partition_and_offset() {
        let id = NodeId::new(300_000_001);
        assert_eq!(id.partition(), PartitionIdx::new(3));
        assert_eq!(id.local_offset(), 1);
    }

    #[test]
    fn root_is_offset_one_and_inside_its_partition() {
        let p = PartitionIdx::new(3);
        assert_eq!(p.root(), NodeId::new(300_000_001));
        assert!(p.contains(p.root()));
    }

    #[test]
    fn node_id_composition_is_bounds_checked() {
        let p = PartitionIdx::new(7);
        assert_eq!(p.node_id(42), Some(NodeId::new(700_000_042)));
        assert_eq!(p.node_id(NODE_CAPACITY), Some(NodeId::new(799_999_999)));
        assert_eq!(p.node_id(0), None); // offset 0 aliases the boundary
        assert_eq!(p.node_id(NODE_CAPACITY + 1), None); // escapes partition
        assert_eq!(p.node_id(-1), None);
    }

    #[test]
    fn compose_and_split_round_trip() {
        let p = PartitionIdx::new(289);
        for offset in [1, 2, 12_345, NODE_CAPACITY] {
            let id = p.node_id(offset).expect("offset is in range");
            assert_eq!(id.partition(), p);
            assert_eq!(id.local_offset(), offset);
        }
    }

    #[test]
    fn contains_distinguishes_partitions() {
        let p3 = PartitionIdx::new(3);
        assert!(p3.contains(NodeId::new(300_000_005)));
        assert!(!p3.contains(NodeId::new(800_000_001)));
    }
}
