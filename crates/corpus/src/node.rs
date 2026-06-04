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
use bookrack_dbkit::{ColumnSpec, ForeignKey, IndexSpec, OnDelete, TableSpec, decode};
use rusqlite::{Connection, OptionalExtension, Row, named_params};

use crate::{Corpus, CorpusError, Result};

/// The single source of truth for the `nodes` table's schema. Its DDL is
/// rendered from this spec, so the schema and the code that reads it
/// cannot drift apart.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "nodes",
    comment: None,
    columns: &[
        ColumnSpec::int("node_id").primary_key(),
        ColumnSpec::int("parent_id").references(ForeignKey::new(
            "nodes",
            "node_id",
            OnDelete::Cascade,
        )),
        ColumnSpec::int("book_root_id").not_null(),
        ColumnSpec::int("ordinal").not_null(),
        ColumnSpec::int("depth").not_null(),
        ColumnSpec::text("node_type").not_null(),
        ColumnSpec::text("title"),
        ColumnSpec::text("text_content"),
        ColumnSpec::int("char_count"),
        ColumnSpec::int("sentence_count"),
        ColumnSpec::int("toc_lo"),
        ColumnSpec::int("toc_hi"),
        ColumnSpec::int("page_index_start"),
        ColumnSpec::int("page_index_end"),
        ColumnSpec::text("stable_anchor"),
        ColumnSpec::text("text_sha256"),
        ColumnSpec::text("norm_text_sha256"),
        ColumnSpec::text("subtree_content_sha256"),
        ColumnSpec::int("expression_id"),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[
        IndexSpec::on("idx_node_root", &["book_root_id", "parent_id", "ordinal"]),
        IndexSpec::on("idx_node_parent", &["parent_id", "ordinal"]),
        IndexSpec::on("idx_node_type", &["node_type"]),
        IndexSpec::on("idx_node_norm_sha", &["norm_text_sha256"])
            .partial("norm_text_sha256 IS NOT NULL"),
        IndexSpec::on("idx_node_subtree_sig", &["subtree_content_sha256"])
            .partial("subtree_content_sha256 IS NOT NULL"),
    ],
};

/// The six organizing [`NodeType`] variants whose rows make up a book's
/// table of contents. Used by [`Corpus::toc_for_book`]'s IN-list.
const ORGANIZING_NODE_TYPES: &[NodeType] = &[
    NodeType::Collection,
    NodeType::Volume,
    NodeType::Work,
    NodeType::Chapter,
    NodeType::Section,
    NodeType::Subsection,
];

/// `INSERT` for one node. Parameters are bound by name, so this column
/// order and the bind list in [`insert_one`] need not be kept in step.
const INSERT_NODE_SQL: &str = "INSERT INTO nodes \
     (node_id, parent_id, book_root_id, ordinal, depth, node_type, \
      title, text_content, char_count, sentence_count, toc_lo, toc_hi, \
      page_index_start, page_index_end, stable_anchor, text_sha256, \
      norm_text_sha256, subtree_content_sha256, expression_id) \
     VALUES (:node_id, :parent_id, :book_root_id, :ordinal, :depth, :node_type, \
             :title, :text_content, :char_count, :sentence_count, :toc_lo, :toc_hi, \
             :page_index_start, :page_index_end, :stable_anchor, :text_sha256, \
             :norm_text_sha256, :subtree_content_sha256, :expression_id)";

/// A `SELECT` of every node column with `tail` (a `WHERE` / `ORDER BY`
/// clause) appended. The column list is derived from [`SPEC`], so it can
/// never drift from the schema.
fn select_sql(tail: &str) -> String {
    format!("SELECT {} FROM nodes {tail}", SPEC.select_list())
}

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
    /// Build a [`Node`] from a row that includes every `nodes` column.
    /// Columns are read by name, so the row's column order is irrelevant.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Node> {
        Ok(Node {
            node_id: NodeId::new(row.get("node_id")?),
            parent_id: row.get::<_, Option<i64>>("parent_id")?.map(NodeId::new),
            book_root_id: NodeId::new(row.get("book_root_id")?),
            ordinal: row.get("ordinal")?,
            depth: row.get("depth")?,
            node_type: decode(row, "node_type", NodeType::from_db_str)?,
            title: row.get("title")?,
            text_content: row.get("text_content")?,
            char_count: row.get("char_count")?,
            sentence_count: row.get("sentence_count")?,
            toc_lo: row.get("toc_lo")?,
            toc_hi: row.get("toc_hi")?,
            page_index_start: row.get("page_index_start")?,
            page_index_end: row.get("page_index_end")?,
            stable_anchor: row.get("stable_anchor")?,
            text_sha256: row.get("text_sha256")?,
            norm_text_sha256: row.get("norm_text_sha256")?,
            subtree_content_sha256: row.get("subtree_content_sha256")?,
            expression_id: row.get("expression_id")?,
        })
    }
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
/// cache. Parameters are bound by name against [`INSERT_NODE_SQL`].
fn insert_one(conn: &Connection, node: &NewNode) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare_cached(INSERT_NODE_SQL)?;
    stmt.execute(named_params![
        ":node_id": node.node_id.get(),
        ":parent_id": node.parent_id.map(NodeId::get),
        ":book_root_id": node.book_root_id.get(),
        ":ordinal": node.ordinal,
        ":depth": node.depth,
        ":node_type": node.node_type.as_str(),
        ":title": node.title,
        ":text_content": node.text_content,
        ":char_count": node.char_count,
        ":sentence_count": node.sentence_count,
        ":toc_lo": node.toc_lo,
        ":toc_hi": node.toc_hi,
        ":page_index_start": node.page_index_start,
        ":page_index_end": node.page_index_end,
        ":stable_anchor": node.stable_anchor,
        ":text_sha256": node.text_sha256,
        ":norm_text_sha256": node.norm_text_sha256,
        ":subtree_content_sha256": node.subtree_content_sha256,
        ":expression_id": node.expression_id,
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
                &select_sql("WHERE node_id = :node_id"),
                named_params! { ":node_id": node_id.get() },
                Node::from_row,
            )
            .optional()?;
        Ok(node)
    }

    /// Fetch a node's direct children, ordered by sibling `ordinal`.
    pub fn children(&self, parent_id: NodeId) -> Result<Vec<Node>> {
        self.query_nodes(
            &select_sql("WHERE parent_id = :parent_id ORDER BY ordinal"),
            named_params! { ":parent_id": parent_id.get() },
        )
    }

    /// Fetch every node of one book, ordered by node id — that is, in
    /// the order their ids were allocated.
    pub fn book_nodes(&self, book_root_id: NodeId) -> Result<Vec<Node>> {
        self.query_nodes(
            &select_sql("WHERE book_root_id = :book_root_id ORDER BY node_id"),
            named_params! { ":book_root_id": book_root_id.get() },
        )
    }

    /// Number of nodes belonging to one book. Uses the same index as
    /// [`Self::book_nodes`] and [`Self::drop_partition`].
    pub fn count_book_nodes(&self, book_root_id: NodeId) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE book_root_id = :book_root_id",
            named_params! { ":book_root_id": book_root_id.get() },
            |row| row.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Find every prose leaf whose normalized text hashes to `hash` —
    /// the inverted lookup behind cross-file content deduplication.
    pub fn find_by_norm_text_sha256(&self, hash: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &select_sql("WHERE norm_text_sha256 = :hash"),
            named_params! { ":hash": hash },
        )
    }

    /// Find every organizing node whose subtree content signature is
    /// `signature` — how matching content is detected across
    /// manifestations.
    pub fn find_by_subtree_content_sha256(&self, signature: &str) -> Result<Vec<Node>> {
        self.query_nodes(
            &select_sql("WHERE subtree_content_sha256 = :signature"),
            named_params! { ":signature": signature },
        )
    }

    /// Fetch the organizing nodes that form one book's table of
    /// contents, ordered as a depth-first TOC walk: by `toc_lo` first
    /// (the start of each node's document-order span) then by `depth`
    /// to place a parent ahead of children that share its start, then
    /// by `ordinal` as a final tiebreaker.
    ///
    /// At most `cap` rows are returned; the cap is enforced inside the
    /// SQL with `LIMIT`. An unknown `book_root_id` returns an empty
    /// `Vec` rather than an error. Leaves are filtered out: the
    /// result is the TOC, not the full node tree.
    pub fn toc_for_book(&self, book_root_id: NodeId, cap: usize) -> Result<Vec<Node>> {
        let placeholders = ORGANIZING_NODE_TYPES
            .iter()
            .map(|t| format!("'{}'", t.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = select_sql(&format!(
            "WHERE book_root_id = :book_root_id \
             AND node_type IN ({placeholders}) \
             ORDER BY toc_lo, depth, ordinal LIMIT :cap"
        ));
        let cap_i = i64::try_from(cap).unwrap_or(i64::MAX);
        self.query_nodes(
            &sql,
            named_params! {
                ":book_root_id": book_root_id.get(),
                ":cap": cap_i,
            },
        )
    }

    /// Set or clear a node's expression link. Returns whether a node
    /// with that id existed.
    pub fn set_expression_id(&self, node_id: NodeId, expression_id: Option<i64>) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE nodes SET expression_id = :expression_id WHERE node_id = :node_id",
            named_params! { ":expression_id": expression_id, ":node_id": node_id.get() },
        )?;
        Ok(affected > 0)
    }

    /// Run a `nodes` query built by [`select_sql`] and collect the rows.
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
    fn a_prose_leaf_round_trips_every_field() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let leaf_id = corpus.allocate_node_ids(idx, 1).expect("ids")[0];

        // Every field a prose leaf may carry is set to a distinct value,
        // so a column dropped from a query or a parameter left unbound
        // shows up as a failed assertion.
        let leaf = NewNode::child(leaf_id, root, root, 7, 1, NodeType::Paragraph)
            .text("Hello, world.")
            .text_stats(13, 1)
            .toc_span(5, 5)
            .pages(2, 3)
            .content_hashes("anchor", "raw-sha", "norm-sha");
        corpus.insert_node(&leaf).expect("insert leaf");

        let read = corpus.get_node(leaf_id).expect("get").expect("present");
        assert_eq!(read.node_id, leaf_id);
        assert_eq!(read.parent_id, Some(root));
        assert_eq!(read.book_root_id, root);
        assert_eq!(read.ordinal, 7);
        assert_eq!(read.depth, 1);
        assert_eq!(read.node_type, NodeType::Paragraph);
        assert_eq!(read.text_content.as_deref(), Some("Hello, world."));
        assert_eq!(read.char_count, Some(13));
        assert_eq!(read.sentence_count, Some(1));
        assert_eq!(read.toc_lo, Some(5));
        assert_eq!(read.toc_hi, Some(5));
        assert_eq!(read.page_index_start, Some(2));
        assert_eq!(read.page_index_end, Some(3));
        assert_eq!(read.stable_anchor.as_deref(), Some("anchor"));
        assert_eq!(read.text_sha256.as_deref(), Some("raw-sha"));
        assert_eq!(read.norm_text_sha256.as_deref(), Some("norm-sha"));
        // A prose leaf carries neither a title nor a subtree signature.
        assert_eq!(read.title, None);
        assert_eq!(read.subtree_content_sha256, None);
        assert_eq!(read.expression_id, None);
    }

    #[test]
    fn an_organizing_node_round_trips_every_field() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let chapter_id = corpus.allocate_node_ids(idx, 1).expect("ids")[0];

        // Covers the columns a prose leaf cannot: the title and the
        // subtree content signature.
        let chapter = NewNode::child(chapter_id, root, root, 4, 1, NodeType::Chapter)
            .title("Chapter One")
            .toc_span(10, 40)
            .pages(8, 25)
            .subtree_signature("subtree-sig");
        corpus.insert_node(&chapter).expect("insert chapter");

        let read = corpus.get_node(chapter_id).expect("get").expect("present");
        assert_eq!(read.node_type, NodeType::Chapter);
        assert_eq!(read.ordinal, 4);
        assert_eq!(read.depth, 1);
        assert_eq!(read.title.as_deref(), Some("Chapter One"));
        assert_eq!(read.toc_lo, Some(10));
        assert_eq!(read.toc_hi, Some(40));
        assert_eq!(read.page_index_start, Some(8));
        assert_eq!(read.page_index_end, Some(25));
        assert_eq!(read.subtree_content_sha256.as_deref(), Some("subtree-sig"));
        // An organizing node carries no body text or prose-leaf hashes.
        assert_eq!(read.text_content, None);
        assert_eq!(read.char_count, None);
        assert_eq!(read.norm_text_sha256, None);
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
    fn toc_for_book_returns_only_organizing_nodes_in_depth_first_order() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let ids = corpus.allocate_node_ids(idx, 5).expect("ids");
        // Two chapters under the root, one section under chapter 1, and
        // one prose leaf under chapter 1 — the leaf must not appear in
        // the TOC.
        let chap1 = ids[0];
        let chap2 = ids[1];
        let sect1 = ids[2];
        let leaf = ids[3];

        corpus
            .insert_node(
                &NewNode::child(chap1, root, root, 0, 1, NodeType::Chapter)
                    .title("Chapter One")
                    .toc_span(1, 50),
            )
            .expect("chap1");
        corpus
            .insert_node(
                &NewNode::child(sect1, chap1, root, 0, 2, NodeType::Section)
                    .title("Section 1.1")
                    .toc_span(2, 20),
            )
            .expect("sect1");
        corpus
            .insert_node(
                &NewNode::child(leaf, chap1, root, 1, 2, NodeType::Paragraph)
                    .text("body")
                    .text_stats(4, 1)
                    .toc_span(21, 21),
            )
            .expect("leaf");
        corpus
            .insert_node(
                &NewNode::child(chap2, root, root, 1, 1, NodeType::Chapter)
                    .title("Chapter Two")
                    .toc_span(60, 99),
            )
            .expect("chap2");

        // Also seed the root's toc_span so it sorts ahead of chapter 1.
        corpus
            .conn
            .execute(
                "UPDATE nodes SET toc_lo = 1, toc_hi = 99 WHERE node_id = ?",
                [root.get()],
            )
            .expect("set root span");

        let toc = corpus.toc_for_book(root, 1000).expect("toc");
        let titles: Vec<&str> = toc
            .iter()
            .map(|n| n.title.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            titles,
            vec!["A Book", "Chapter One", "Section 1.1", "Chapter Two"]
        );
        assert!(
            toc.iter().all(|n| n.node_type.is_organizing()),
            "leaves must be filtered out: {toc:?}"
        );
    }

    #[test]
    fn toc_for_book_caps_the_result_size() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let (idx, root) = seed_book(&mut corpus, 1);
        let ids = corpus.allocate_node_ids(idx, 4).expect("ids");
        for (i, id) in ids.iter().enumerate() {
            corpus
                .insert_node(
                    &NewNode::child(*id, root, root, i as i64, 1, NodeType::Chapter)
                        .title(format!("Chapter {i}"))
                        .toc_span((i as i64) * 10 + 5, (i as i64) * 10 + 9),
                )
                .expect("chapter");
        }
        let toc = corpus.toc_for_book(root, 2).expect("toc");
        assert_eq!(toc.len(), 2);
    }

    #[test]
    fn toc_for_book_unknown_root_returns_empty() {
        let corpus = Corpus::open_in_memory().expect("open");
        let toc = corpus
            .toc_for_book(NodeId::new(999_999_999), 100)
            .expect("toc");
        assert!(toc.is_empty());
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
