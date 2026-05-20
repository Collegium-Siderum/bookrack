// SPDX-License-Identifier: Apache-2.0

//! The node tree: the [`Node`] row, the [`NewNode`] write payload, and
//! the tree's read/write operations on [`Corpus`].
//!
//! A node is one position in a book's tree. Organizing nodes
//! (collection, work, chapter, ...) form the structure above the
//! leaves; prose leaves carry searchable body text; structural leaves
//! carry non-prose page artifacts. Which fields a node may populate is
//! decided by its [`NodeType`] group — an invariant enforced here, at
//! the write boundary, so a malformed node never reaches the store.

use bookrack_core::{NodeId, NodeType};
use rusqlite::{Connection, OptionalExtension, Row};

use crate::{Corpus, CorpusError, Result};

/// Column list shared by every `nodes` `SELECT`. Its order is the
/// contract between the queries and [`Node::from_row`] — keep them in
/// step.
const NODE_COLUMNS: &str = "node_id, parent_id, book_root_id, ordinal, depth, node_type, \
     title, text_content, char_count, sentence_count, toc_lo, toc_hi, \
     page_index_start, page_index_end, stable_anchor, text_sha256, \
     norm_text_sha256, subtree_content_sha256, expression_id";

/// `INSERT` for one node. Column order matches the bind list in
/// [`insert_one`].
const INSERT_NODE_SQL: &str = "INSERT INTO nodes \
     (node_id, parent_id, book_root_id, ordinal, depth, node_type, \
      title, text_content, char_count, sentence_count, toc_lo, toc_hi, \
      page_index_start, page_index_end, stable_anchor, text_sha256, \
      norm_text_sha256, subtree_content_sha256, expression_id) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, \
             ?15, ?16, ?17, ?18, ?19)";

/// A node read back from `corpus.db` — one full `nodes` row.
///
/// Whether a given optional field is populated follows from
/// [`Node::node_type`]'s group; see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Global node id; encodes the owning book's partition.
    pub node_id: NodeId,
    /// Parent in the tree; `None` only for a book root.
    pub parent_id: Option<NodeId>,
    /// Root node of the owning book.
    pub book_root_id: NodeId,
    /// Position among siblings under the same parent.
    pub ordinal: i64,
    /// Tree depth; 0 is the book root.
    pub depth: i64,
    /// The node's type, fixing which group it belongs to.
    pub node_type: NodeType,
    /// Heading text of an organizing node; `None` on leaves.
    pub title: Option<String>,
    /// Body text of a leaf; `None` on organizing nodes.
    pub text_content: Option<String>,
    /// Character count of a leaf's body text.
    pub char_count: Option<i64>,
    /// Sentence count of a leaf's body text.
    pub sentence_count: Option<i64>,
    /// Low end of the document-order span this node covers. A leaf
    /// occupies a single position, so `toc_lo == toc_hi`.
    pub toc_lo: Option<i64>,
    /// High end of the document-order span this node covers.
    pub toc_hi: Option<i64>,
    /// First source page this node was drawn from.
    pub page_index_start: Option<i64>,
    /// Last source page this node was drawn from.
    pub page_index_end: Option<i64>,
    /// Short prefix hash of a prose leaf, for approximate location.
    pub stable_anchor: Option<String>,
    /// SHA-256 of a prose leaf's raw bytes.
    pub text_sha256: Option<String>,
    /// SHA-256 of a prose leaf's normalized text — the cross-file
    /// content-deduplication key.
    pub norm_text_sha256: Option<String>,
    /// Content signature of an organizing node's subtree.
    pub subtree_content_sha256: Option<String>,
    /// Soft reference to a `catalog.db` expression, backfilled when the
    /// node's content is recognized across manifestations.
    pub expression_id: Option<i64>,
}

impl Node {
    /// Build a [`Node`] from a row whose columns are [`NODE_COLUMNS`].
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Node> {
        Ok(Node {
            node_id: NodeId::new(row.get(0)?),
            parent_id: row.get::<_, Option<i64>>(1)?.map(NodeId::new),
            book_root_id: NodeId::new(row.get(2)?),
            ordinal: row.get(3)?,
            depth: row.get(4)?,
            node_type: node_type_at(row, 5)?,
            title: row.get(6)?,
            text_content: row.get(7)?,
            char_count: row.get(8)?,
            sentence_count: row.get(9)?,
            toc_lo: row.get(10)?,
            toc_hi: row.get(11)?,
            page_index_start: row.get(12)?,
            page_index_end: row.get(13)?,
            stable_anchor: row.get(14)?,
            text_sha256: row.get(15)?,
            norm_text_sha256: row.get(16)?,
            subtree_content_sha256: row.get(17)?,
            expression_id: row.get(18)?,
        })
    }
}

/// Read a `node_type` cell and decode it to a [`NodeType`]. An
/// unrecognized string means the database was written by something
/// other than this crate; surface it as a conversion failure.
fn node_type_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<NodeType> {
    let raw: String = row.get(idx)?;
    NodeType::from_db_str(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown node_type {raw:?}"),
            )),
        )
    })
}

/// A node about to be written to `corpus.db`.
///
/// Start from [`NewNode::root`] or [`NewNode::child`], then attach the
/// optional fields with the builder methods. Group invariants — which
/// fields a node type may carry — are checked when the node is inserted,
/// not as it is built, so a half-built node is never itself invalid.
#[derive(Debug, Clone)]
pub struct NewNode {
    node_id: NodeId,
    parent_id: Option<NodeId>,
    book_root_id: NodeId,
    ordinal: i64,
    depth: i64,
    node_type: NodeType,
    title: Option<String>,
    text_content: Option<String>,
    char_count: Option<i64>,
    sentence_count: Option<i64>,
    toc_lo: Option<i64>,
    toc_hi: Option<i64>,
    page_index_start: Option<i64>,
    page_index_end: Option<i64>,
    stable_anchor: Option<String>,
    text_sha256: Option<String>,
    norm_text_sha256: Option<String>,
    subtree_content_sha256: Option<String>,
    expression_id: Option<i64>,
}

impl NewNode {
    /// A book's root node. Its id *is* the book root id; it has no
    /// parent and sits at depth 0.
    pub fn root(book_root_id: NodeId, node_type: NodeType) -> NewNode {
        NewNode::bare(book_root_id, None, book_root_id, 0, 0, node_type)
    }

    /// A non-root node, at `ordinal` among its siblings and `depth`
    /// below the book root. Its `node_id` must come from
    /// [`Corpus::allocate_node_ids`] so it lands in the book's
    /// partition.
    pub fn child(
        node_id: NodeId,
        parent_id: NodeId,
        book_root_id: NodeId,
        ordinal: i64,
        depth: i64,
        node_type: NodeType,
    ) -> NewNode {
        NewNode::bare(
            node_id,
            Some(parent_id),
            book_root_id,
            ordinal,
            depth,
            node_type,
        )
    }

    /// Shared constructor: required fields set, every optional cleared.
    fn bare(
        node_id: NodeId,
        parent_id: Option<NodeId>,
        book_root_id: NodeId,
        ordinal: i64,
        depth: i64,
        node_type: NodeType,
    ) -> NewNode {
        NewNode {
            node_id,
            parent_id,
            book_root_id,
            ordinal,
            depth,
            node_type,
            title: None,
            text_content: None,
            char_count: None,
            sentence_count: None,
            toc_lo: None,
            toc_hi: None,
            page_index_start: None,
            page_index_end: None,
            stable_anchor: None,
            text_sha256: None,
            norm_text_sha256: None,
            subtree_content_sha256: None,
            expression_id: None,
        }
    }

    /// Set the heading text — for organizing nodes only.
    pub fn title(mut self, title: impl Into<String>) -> NewNode {
        self.title = Some(title.into());
        self
    }

    /// Set the body text — for leaves only.
    pub fn text(mut self, text: impl Into<String>) -> NewNode {
        self.text_content = Some(text.into());
        self
    }

    /// Set the leaf body-text statistics.
    pub fn text_stats(mut self, char_count: i64, sentence_count: i64) -> NewNode {
        self.char_count = Some(char_count);
        self.sentence_count = Some(sentence_count);
        self
    }

    /// Set the document-order span this node covers. A leaf passes the
    /// same value for both ends.
    pub fn toc_span(mut self, lo: i64, hi: i64) -> NewNode {
        self.toc_lo = Some(lo);
        self.toc_hi = Some(hi);
        self
    }

    /// Set the inclusive range of source pages this node was drawn from.
    pub fn pages(mut self, start: i64, end: i64) -> NewNode {
        self.page_index_start = Some(start);
        self.page_index_end = Some(end);
        self
    }

    /// Set the prose-leaf content hashes: the approximate-location
    /// anchor, the raw-byte hash, and the normalized-text hash that
    /// keys cross-file deduplication. For prose leaves only.
    pub fn content_hashes(
        mut self,
        stable_anchor: impl Into<String>,
        text_sha256: impl Into<String>,
        norm_text_sha256: impl Into<String>,
    ) -> NewNode {
        self.stable_anchor = Some(stable_anchor.into());
        self.text_sha256 = Some(text_sha256.into());
        self.norm_text_sha256 = Some(norm_text_sha256.into());
        self
    }

    /// Set the subtree content signature — for organizing nodes only.
    pub fn subtree_signature(mut self, signature: impl Into<String>) -> NewNode {
        self.subtree_content_sha256 = Some(signature.into());
        self
    }

    /// Link the node to a `catalog.db` expression.
    pub fn expression_id(mut self, expression_id: i64) -> NewNode {
        self.expression_id = Some(expression_id);
        self
    }

    /// Check the structural invariants a node must satisfy. Run at the
    /// write boundary; see the module documentation for the rules.
    fn validate(&self) -> Result<()> {
        let reject = |reason: &'static str| CorpusError::InvalidNode {
            node_id: self.node_id.get(),
            reason,
        };

        if self.depth < 0 || self.ordinal < 0 {
            return Err(reject("depth and ordinal must be non-negative"));
        }
        // Invariant I2: a node always lives in its own book's partition.
        if self.node_id.partition() != self.book_root_id.partition() {
            return Err(reject("node id lies outside its book's partition"));
        }
        // The depth-0 root is the one and only parentless node.
        if self.parent_id.is_none() != (self.depth == 0) {
            return Err(reject("only the depth-0 root may be parentless"));
        }

        let organizing = self.node_type.is_organizing();
        let prose_leaf = self.node_type.is_prose_leaf();

        if self.title.is_some() && !organizing {
            return Err(reject("title is reserved for organizing nodes"));
        }
        if self.subtree_content_sha256.is_some() && !organizing {
            return Err(reject(
                "subtree content signature is reserved for organizing nodes",
            ));
        }
        if organizing
            && (self.text_content.is_some()
                || self.char_count.is_some()
                || self.sentence_count.is_some())
        {
            return Err(reject("organizing nodes carry no body text"));
        }
        let has_content_hash = self.stable_anchor.is_some()
            || self.text_sha256.is_some()
            || self.norm_text_sha256.is_some();
        if has_content_hash && !prose_leaf {
            return Err(reject("content hashes are reserved for prose leaves"));
        }
        if let (Some(lo), Some(hi)) = (self.toc_lo, self.toc_hi)
            && lo > hi
        {
            return Err(reject("toc span is inverted"));
        }
        Ok(())
    }
}

/// Insert one already-validated node through the connection's statement
/// cache. The bind order matches [`INSERT_NODE_SQL`].
fn insert_one(conn: &Connection, node: &NewNode) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare_cached(INSERT_NODE_SQL)?;
    stmt.execute(rusqlite::params![
        node.node_id.get(),
        node.parent_id.map(NodeId::get),
        node.book_root_id.get(),
        node.ordinal,
        node.depth,
        node.node_type.as_str(),
        node.title,
        node.text_content,
        node.char_count,
        node.sentence_count,
        node.toc_lo,
        node.toc_hi,
        node.page_index_start,
        node.page_index_end,
        node.stable_anchor,
        node.text_sha256,
        node.norm_text_sha256,
        node.subtree_content_sha256,
        node.expression_id,
    ])?;
    Ok(())
}

impl Corpus {
    /// Insert a single node. Fails with [`CorpusError::InvalidNode`] if
    /// it breaks a structural invariant, or with a database error if its
    /// `parent_id` does not resolve to an existing node.
    pub fn insert_node(&self, node: &NewNode) -> Result<()> {
        node.validate()?;
        insert_one(&self.conn, node)?;
        Ok(())
    }

    /// Insert many nodes as one atomic batch.
    ///
    /// Every node is validated before any is written. Foreign-key
    /// checks are deferred to commit time, so the slice may list nodes
    /// in any order — a child ahead of its parent is fine — as long as
    /// every `parent_id` resolves once the whole batch is in place.
    pub fn insert_nodes(&mut self, nodes: &[NewNode]) -> Result<()> {
        for node in nodes {
            node.validate()?;
        }
        let tx = self.conn.transaction()?;
        // Deferring lets a batch carry a whole subtree in arbitrary
        // order; the pragma resets when the transaction ends.
        tx.pragma_update(None, "defer_foreign_keys", "ON")?;
        for node in nodes {
            insert_one(&tx, node)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Fetch one node by id, or `None` if no such node exists.
    pub fn get_node(&self, node_id: NodeId) -> Result<Option<Node>> {
        let node = self
            .conn
            .query_row(
                &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE node_id = ?1"),
                [node_id.get()],
                Node::from_row,
            )
            .optional()?;
        Ok(node)
    }

    /// Fetch a node's direct children, ordered by sibling `ordinal`.
    pub fn children(&self, parent_id: NodeId) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE parent_id = ?1 ORDER BY ordinal"),
            [parent_id.get()],
        )
    }

    /// Fetch every node of one book, ordered by node id — that is, in
    /// the order their ids were allocated.
    pub fn book_nodes(&self, book_root_id: NodeId) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE book_root_id = ?1 ORDER BY node_id"),
            [book_root_id.get()],
        )
    }

    /// Find every prose leaf whose normalized text hashes to `hash` —
    /// the inverted lookup behind cross-file content deduplication.
    pub fn find_by_norm_text_sha256(&self, hash: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE norm_text_sha256 = ?1"),
            [hash],
        )
    }

    /// Find every organizing node whose subtree content signature is
    /// `signature` — how matching content is detected across
    /// manifestations.
    pub fn find_by_subtree_content_sha256(&self, signature: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE subtree_content_sha256 = ?1"),
            [signature],
        )
    }

    /// Set or clear a node's expression link. Returns whether a node
    /// with that id existed.
    pub fn set_expression_id(&self, node_id: NodeId, expression_id: Option<i64>) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE nodes SET expression_id = ?1 WHERE node_id = ?2",
            (expression_id, node_id.get()),
        )?;
        Ok(affected > 0)
    }

    /// Run a `nodes` query whose `SELECT` list is [`NODE_COLUMNS`] and
    /// collect the rows.
    fn query_nodes(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(sql)?;
        let nodes = stmt
            .query_map(params, Node::from_row)?
            .collect::<rusqlite::Result<Vec<Node>>>()?;
        Ok(nodes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_core::PartitionIdx;

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
    fn a_node_round_trips_through_the_store() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let leaf_id = corpus.allocate_node_ids(idx, 1).expect("ids")[0];

        let leaf = NewNode::child(leaf_id, root, root, 0, 1, NodeType::Paragraph)
            .text("Hello, world.")
            .text_stats(13, 1)
            .toc_span(5, 5)
            .pages(2, 2)
            .content_hashes("anchor", "raw-sha", "norm-sha");
        corpus.insert_node(&leaf).expect("insert leaf");

        let read = corpus.get_node(leaf_id).expect("get").expect("present");
        assert_eq!(read.node_id, leaf_id);
        assert_eq!(read.parent_id, Some(root));
        assert_eq!(read.book_root_id, root);
        assert_eq!(read.node_type, NodeType::Paragraph);
        assert_eq!(read.text_content.as_deref(), Some("Hello, world."));
        assert_eq!(read.char_count, Some(13));
        assert_eq!(read.toc_lo, Some(5));
        assert_eq!(read.toc_hi, Some(5));
        assert_eq!(read.norm_text_sha256.as_deref(), Some("norm-sha"));
    }

    #[test]
    fn a_missing_node_reads_as_none() {
        let corpus = Corpus::open_in_memory().expect("open");
        assert_eq!(corpus.get_node(NodeId::new(123)).expect("get"), None);
    }

    #[test]
    fn children_come_back_in_ordinal_order() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let ids = corpus.allocate_node_ids(idx, 3).expect("ids");

        // Insert the children out of ordinal order to prove the query
        // sorts rather than relying on insertion order.
        for (ordinal, id) in [(2, ids[0]), (0, ids[1]), (1, ids[2])] {
            corpus
                .insert_node(&NewNode::child(
                    id,
                    root,
                    root,
                    ordinal,
                    1,
                    NodeType::Chapter,
                ))
                .expect("insert child");
        }
        let children = corpus.children(root).expect("children");
        let ordinals: Vec<i64> = children.iter().map(|n| n.ordinal).collect();
        assert_eq!(ordinals, vec![0, 1, 2]);
    }

    #[test]
    fn a_batch_may_list_children_before_their_parent() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let partition = corpus.allocate_partition(1).expect("partition");
        let root = partition.book_root_id;
        let child_id = corpus.allocate_node_ids(partition.idx, 1).expect("ids")[0];

        // Child first, parent second: deferred foreign keys make the
        // batch order irrelevant.
        let batch = [
            NewNode::child(child_id, root, root, 0, 1, NodeType::Chapter),
            NewNode::root(root, NodeType::Work),
        ];
        corpus.insert_nodes(&batch).expect("insert batch");
        assert_eq!(corpus.book_nodes(root).expect("book nodes").len(), 2);
    }

    #[test]
    fn a_child_with_no_real_parent_is_refused() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let ids = corpus.allocate_node_ids(idx, 2).expect("ids");
        // Point at an id that was reserved but never inserted.
        let orphan = NewNode::child(ids[0], ids[1], root, 0, 1, NodeType::Chapter);
        assert!(matches!(
            corpus.insert_node(&orphan),
            Err(CorpusError::Sqlite(_))
        ));
    }

    #[test]
    fn the_norm_text_hash_is_an_inverted_index_across_books() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let shared = "shared-norm-hash";

        // The same passage in two different books shares one hash.
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
        let hits = corpus.find_by_norm_text_sha256(shared).expect("lookup");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|n| n.node_type == NodeType::Paragraph));
    }

    #[test]
    fn an_expression_link_can_be_set_and_cleared() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let leaf_id = corpus.allocate_node_ids(idx, 1).expect("ids")[0];
        corpus
            .insert_node(&NewNode::child(
                leaf_id,
                root,
                root,
                0,
                1,
                NodeType::Paragraph,
            ))
            .expect("insert");

        assert!(corpus.set_expression_id(leaf_id, Some(77)).expect("set"));
        assert_eq!(
            corpus
                .get_node(leaf_id)
                .expect("get")
                .unwrap()
                .expression_id,
            Some(77)
        );
        assert!(corpus.set_expression_id(leaf_id, None).expect("clear"));
        assert_eq!(
            corpus
                .get_node(leaf_id)
                .expect("get")
                .unwrap()
                .expression_id,
            None
        );
        // No such node: nothing updated.
        assert!(
            !corpus
                .set_expression_id(NodeId::new(9), Some(1))
                .expect("miss")
        );
    }

    #[test]
    fn dropping_a_partition_clears_its_whole_tree() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let ids = corpus.allocate_node_ids(idx, 2).expect("ids");
        for &id in &ids {
            corpus
                .insert_node(&NewNode::child(id, root, root, 0, 1, NodeType::Chapter))
                .expect("insert");
        }
        assert_eq!(corpus.book_nodes(root).expect("nodes").len(), 3);

        corpus.drop_partition(idx).expect("drop");
        assert!(corpus.book_nodes(root).expect("nodes").is_empty());
        assert_eq!(corpus.get_node(root).expect("get"), None);
    }

    #[test]
    fn the_partition_invariant_is_enforced_on_insert() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (_, root) = seed_book(&mut corpus, 1);
        // A node id from a different partition than the book root.
        let foreign = PartitionIdx::new(99).node_id(2).unwrap();
        let bad = NewNode::child(foreign, root, root, 0, 1, NodeType::Chapter);
        assert!(matches!(
            corpus.insert_node(&bad),
            Err(CorpusError::InvalidNode { .. })
        ));
    }

    #[test]
    fn group_field_invariants_are_enforced_on_insert() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let ids = corpus.allocate_node_ids(idx, 4).expect("ids");

        // A prose leaf may not carry an organizing-node title.
        let titled_leaf =
            NewNode::child(ids[0], root, root, 0, 1, NodeType::Paragraph).title("nope");
        assert!(matches!(
            corpus.insert_node(&titled_leaf),
            Err(CorpusError::InvalidNode { .. })
        ));

        // An organizing node may not carry prose-leaf content hashes.
        let hashed_chapter = NewNode::child(ids[1], root, root, 0, 1, NodeType::Chapter)
            .content_hashes("a", "b", "c");
        assert!(matches!(
            corpus.insert_node(&hashed_chapter),
            Err(CorpusError::InvalidNode { .. })
        ));

        // An inverted document-order span is rejected.
        let inverted = NewNode::child(ids[2], root, root, 0, 1, NodeType::Paragraph).toc_span(9, 4);
        assert!(matches!(
            corpus.insert_node(&inverted),
            Err(CorpusError::InvalidNode { .. })
        ));

        // A non-root node claiming depth 0 contradicts its parentage.
        let bad_depth = NewNode::child(ids[3], root, root, 0, 0, NodeType::Chapter);
        assert!(matches!(
            corpus.insert_node(&bad_depth),
            Err(CorpusError::InvalidNode { .. })
        ));
    }
}
