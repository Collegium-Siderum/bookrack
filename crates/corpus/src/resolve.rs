// SPDX-License-Identifier: Apache-2.0

//! Resolving logical addresses against the node tree.
//!
//! A curated reference is stored as a content-stable logical address
//! `(intake_id, [`Scope`])`, never a bare physical node id. This module
//! is the bridge between the two: [`Corpus::resolve`] turns an address
//! into the physical [`NodeId`] it names within a book's partition, and
//! [`Corpus::address_of`] runs the reverse, producing the address that
//! curation must store for a given node.
//!
//! The address forms map onto the three node groups: a book root is
//! pure partition arithmetic, an organizing node is found by its subtree
//! content signature, and a prose leaf by its normalized-text hash. A
//! `node:` hash that matches nothing is a *broken* reference — distinct
//! from the whole book being gone — which batch 4's dirty-partition work
//! re-anchors.

use bookrack_core::{NodeId, PartitionIdx, Scope};

use crate::{Corpus, CorpusError, Node};

/// Why resolving an address to a node id, or a node id to an address,
/// failed. Distinct variants keep "the whole book is gone" apart from
/// "the book is here but that passage changed" — the broken-reference
/// case batch 4's dirty-partition work re-anchors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// The intake owns no partition: the book does not exist at all.
    #[error("intake {0} owns no node-id partition")]
    UnknownIntake(i64),

    /// A `work:` signature matched no organizing node in this book.
    #[error("intake {intake_id} has no organizing node with subtree signature {sig:?}")]
    WorkNotFound {
        /// The book the address named.
        intake_id: i64,
        /// The subtree content signature that matched nothing.
        sig: String,
    },

    /// A `node:` hash matched no prose leaf in this book — a broken
    /// reference: the book is present but that passage changed.
    #[error("intake {intake_id} has no prose leaf with normalized-text hash {hash:?}")]
    NodeContentGone {
        /// The book the address named.
        intake_id: i64,
        /// The normalized-text hash that matched nothing.
        hash: String,
    },

    /// The signature matched more than one node in this book; a stronger
    /// anchor is needed. Left for the post-launch fuzzy re-anchor work.
    #[error(
        "scope {scope} matches {hits} nodes in intake {intake_id}; a stronger anchor is needed"
    )]
    Ambiguous {
        /// The book the address named.
        intake_id: i64,
        /// The address that resolved to more than one node.
        scope: String,
        /// How many nodes it matched (at least two).
        hits: usize,
    },

    /// `address_of` reached a node with no stable content address — a
    /// structural leaf carries neither signature. Not a curation target
    /// in v1.
    #[error("node {node_id} carries no stable content address")]
    Unaddressable {
        /// The node that cannot be addressed.
        node_id: i64,
    },

    /// The underlying `corpus.db` layer reported an error.
    #[error(transparent)]
    Db(#[from] CorpusError),
}

/// Keep only hits inside this book's partition, then branch on count:
/// exactly one → that id; many → [`ResolveError::Ambiguous`]; none →
/// `Ok(None)` so the caller chooses the right "not found" variant.
fn pick_in_partition(
    partition: PartitionIdx,
    hits: Vec<Node>,
    scope: &Scope,
    intake_id: i64,
) -> Result<Option<NodeId>, ResolveError> {
    let mut mine = hits.into_iter().filter(|n| partition.contains(n.node_id));
    match (mine.next(), mine.next()) {
        (None, _) => Ok(None),
        (Some(only), None) => Ok(Some(only.node_id)),
        // ">= 2"; the exact count past two is not needed for the v1 decision.
        (Some(_), Some(_)) => Err(ResolveError::Ambiguous {
            intake_id,
            scope: scope.to_string(),
            hits: 2,
        }),
    }
}

impl Corpus {
    /// Resolve a logical address `(intake_id, scope)` to the physical
    /// node id it names, within that book's partition.
    ///
    /// `Scope::Book` is pure arithmetic and fails only with
    /// [`ResolveError::UnknownIntake`]. A `work:` / `node:` signature is
    /// looked up, filtered to this book's partition, then resolves to the
    /// single matching id, or fails with the form-specific "not found"
    /// variant for zero hits and [`ResolveError::Ambiguous`] for more
    /// than one.
    pub fn resolve(&self, intake_id: i64, scope: &Scope) -> Result<NodeId, ResolveError> {
        let partition = PartitionIdx::new(intake_id);
        match scope {
            Scope::Book => {
                if self.partition_for_intake(intake_id)?.is_none() {
                    return Err(ResolveError::UnknownIntake(intake_id));
                }
                Ok(partition.root())
            }
            Scope::Work(sig) => {
                let hits = self.find_by_subtree_content_sha256(sig)?;
                pick_in_partition(partition, hits, scope, intake_id)?.ok_or_else(|| {
                    ResolveError::WorkNotFound {
                        intake_id,
                        sig: sig.clone(),
                    }
                })
            }
            Scope::Node(hash) => {
                let hits = self.find_by_norm_text_sha256(hash)?;
                pick_in_partition(partition, hits, scope, intake_id)?.ok_or_else(|| {
                    ResolveError::NodeContentGone {
                        intake_id,
                        hash: hash.clone(),
                    }
                })
            }
        }
    }

    /// The reverse of [`Corpus::resolve`]: given a physical node id,
    /// produce the stable logical address `(intake_id, scope)` curation
    /// must store instead of the bare id.
    ///
    /// The book root maps to `Scope::Book`, an organizing node to its
    /// `work:` subtree signature, and a prose leaf to its `node:`
    /// normalized-text hash. A structural leaf, or a node missing the
    /// signature its group should carry, fails with
    /// [`ResolveError::Unaddressable`] — it is not a curation target.
    pub fn address_of(&self, node_id: NodeId) -> Result<(i64, Scope), ResolveError> {
        let intake_id = node_id.partition().get();
        let node = self.get_node(node_id)?.ok_or(ResolveError::Unaddressable {
            node_id: node_id.get(),
        })?;

        let unaddressable = || ResolveError::Unaddressable {
            node_id: node_id.get(),
        };
        let scope = if node.depth == 0 {
            Scope::Book
        } else if node.node_type.is_organizing() {
            Scope::Work(node.subtree_content_sha256.ok_or_else(unaddressable)?)
        } else if node.node_type.is_prose_leaf() {
            Scope::Node(node.norm_text_sha256.ok_or_else(unaddressable)?)
        } else {
            return Err(unaddressable());
        };
        Ok((intake_id, scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewNode;
    use bookrack_core::NodeType;

    /// Allocate a partition and write its root node, returning the
    /// partition index and the book root id.
    fn seed_book(corpus: &mut Corpus, intake_id: i64) -> (PartitionIdx, NodeId) {
        let partition = corpus.allocate_partition(intake_id).expect("partition");
        corpus
            .insert_node(&NewNode::root(partition.book_root_id, NodeType::Work).title("A Book"))
            .expect("insert root");
        (partition.idx, partition.book_root_id)
    }

    #[test]
    fn the_book_root_round_trips() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (_, root) = seed_book(&mut corpus, 1);

        let (intake_id, scope) = corpus.address_of(root).expect("address");
        assert_eq!(intake_id, 1);
        assert_eq!(scope, Scope::Book);
        assert_eq!(corpus.resolve(intake_id, &scope).expect("resolve"), root);
    }

    #[test]
    fn an_organizing_node_round_trips() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let chapter_id = corpus.allocate_node_ids(idx, 1).expect("ids")[0];
        corpus
            .insert_node(
                &NewNode::child(chapter_id, root, root, 0, 1, NodeType::Chapter)
                    .subtree_signature("subtree-sig"),
            )
            .expect("insert chapter");

        let (intake_id, scope) = corpus.address_of(chapter_id).expect("address");
        assert_eq!(scope, Scope::Work("subtree-sig".to_string()));
        assert_eq!(
            corpus.resolve(intake_id, &scope).expect("resolve"),
            chapter_id
        );
    }

    #[test]
    fn a_prose_leaf_round_trips() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let leaf_id = corpus.allocate_node_ids(idx, 1).expect("ids")[0];
        corpus
            .insert_node(
                &NewNode::child(leaf_id, root, root, 0, 1, NodeType::Paragraph)
                    .text("Hello.")
                    .content_hashes("anchor", "raw", "norm-hash"),
            )
            .expect("insert leaf");

        let (intake_id, scope) = corpus.address_of(leaf_id).expect("address");
        assert_eq!(scope, Scope::Node("norm-hash".to_string()));
        assert_eq!(corpus.resolve(intake_id, &scope).expect("resolve"), leaf_id);
    }

    #[test]
    fn an_unallocated_intake_resolves_to_unknown_intake() {
        let corpus = Corpus::open_in_memory().expect("open");
        let err = corpus.resolve(99, &Scope::Book).expect_err("no such book");
        assert!(matches!(err, ResolveError::UnknownIntake(99)));
    }

    #[test]
    fn an_absent_work_signature_is_work_not_found() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        seed_book(&mut corpus, 1);
        let err = corpus
            .resolve(1, &Scope::Work("missing".to_string()))
            .expect_err("no such work");
        assert!(matches!(
            err,
            ResolveError::WorkNotFound { intake_id: 1, .. }
        ));
    }

    #[test]
    fn an_absent_node_hash_is_node_content_gone() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        seed_book(&mut corpus, 1);
        let err = corpus
            .resolve(1, &Scope::Node("missing".to_string()))
            .expect_err("broken reference");
        assert!(matches!(
            err,
            ResolveError::NodeContentGone { intake_id: 1, .. }
        ));
    }

    #[test]
    fn a_signature_shared_within_one_book_is_ambiguous() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let ids = corpus.allocate_node_ids(idx, 2).expect("ids");
        // Two prose leaves in the same book carry the same normalized
        // text, so the hash alone cannot single one out.
        for (ordinal, id) in [(0, ids[0]), (1, ids[1])] {
            corpus
                .insert_node(
                    &NewNode::child(id, root, root, ordinal, 1, NodeType::Paragraph)
                        .text("Same text")
                        .content_hashes("anchor", "raw", "dup-hash"),
                )
                .expect("insert leaf");
        }
        let err = corpus
            .resolve(1, &Scope::Node("dup-hash".to_string()))
            .expect_err("two matches");
        assert!(matches!(err, ResolveError::Ambiguous { intake_id: 1, .. }));
    }

    #[test]
    fn a_structural_leaf_is_unaddressable() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let table_id = corpus.allocate_node_ids(idx, 1).expect("ids")[0];
        corpus
            .insert_node(&NewNode::child(table_id, root, root, 0, 1, NodeType::Table))
            .expect("insert table");
        let err = corpus.address_of(table_id).expect_err("no stable address");
        assert!(matches!(err, ResolveError::Unaddressable { .. }));
    }

    #[test]
    fn resolution_does_not_leak_across_books() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let shared = "shared-norm-hash";
        // The same passage sits in two different books under one hash.
        for intake in [1, 2] {
            let (idx, root) = seed_book(&mut corpus, intake);
            let leaf_id = corpus.allocate_node_ids(idx, 1).expect("ids")[0];
            corpus
                .insert_node(
                    &NewNode::child(leaf_id, root, root, 0, 1, NodeType::Paragraph)
                        .text("Same text")
                        .content_hashes("anchor", "raw", shared),
                )
                .expect("insert leaf");
        }
        // Resolving in book 1 returns the leaf inside book 1's partition,
        // never the identical passage in book 2.
        let hit = corpus
            .resolve(1, &Scope::Node(shared.to_string()))
            .expect("resolve");
        assert!(PartitionIdx::new(1).contains(hit));
    }
}
