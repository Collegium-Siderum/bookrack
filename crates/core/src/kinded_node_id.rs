// SPDX-License-Identifier: Apache-2.0

//! A node identifier paired with the pipeline it belongs to.
//!
//! Reads addressed by a bare `node_id` are ambiguous: book ingest and
//! paper glean each own their own corpus database, and the same numeric
//! id may exist in both. `KindedNodeId` puts the pipeline tag next to
//! the id so a read API can route to the correct corpus from the
//! signature alone — what used to silently land in the books corpus is
//! now a type-checked routing decision at the call site.

use serde::Serialize;

use crate::ItemKind;
use crate::NodeId;

/// A node identifier together with the pipeline kind that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct KindedNodeId {
    /// Which pipeline (book ingest or paper glean) the node was
    /// produced by, and therefore which corpus database holds its row.
    pub kind: ItemKind,
    /// The node identifier within that corpus.
    pub node_id: NodeId,
}

impl KindedNodeId {
    /// A node from the books corpus.
    pub const fn book(node_id: NodeId) -> Self {
        Self {
            kind: ItemKind::Book,
            node_id,
        }
    }

    /// A node from the papers corpus.
    pub const fn paper(node_id: NodeId) -> Self {
        Self {
            kind: ItemKind::Paper,
            node_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_and_paper_constructors_set_the_corresponding_kind() {
        let id = NodeId::new(100_000_001);
        assert_eq!(KindedNodeId::book(id).kind, ItemKind::Book);
        assert_eq!(KindedNodeId::paper(id).kind, ItemKind::Paper);
        assert_eq!(KindedNodeId::book(id).node_id, id);
    }

    #[test]
    fn serializes_as_a_flat_kind_node_id_object() {
        let target = KindedNodeId::paper(NodeId::new(100_000_042));
        let v = serde_json::to_value(target).expect("serialize");
        assert_eq!(v["kind"], "paper");
        assert_eq!(v["node_id"], 100_000_042);
    }
}
