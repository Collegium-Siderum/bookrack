// SPDX-License-Identifier: Apache-2.0

//! STRUCTURE: lift one [`Extraction`] into a corpus node tree.
//!
//! The work is split in two. [`plan_tree`] is pure: it turns the flat
//! blocks and the flattened, depth-tagged table of contents into an
//! ordered list of [`PlannedNode`]s — the organizing tree, the prose and
//! structural leaves, the document-order intervals, and the content
//! hashes — with every parent reference held as an index into that list.
//! [`TreePlan::into_new_nodes`] then binds those indices to the node ids
//! the corpus allocator hands out, producing [`NewNode`]s ready to
//! insert. Keeping the algorithm free of the database makes it testable
//! against synthetic extractions with no I/O.
//!
//! Tree shape rules:
//! - The book root is the one organizing node at depth 0.
//! - Organizing nodes come from TOC entries; TOC depth picks the type
//!   (0 -> Chapter, 1 -> Section, deeper -> Subsection) and the parent is
//!   resolved with a depth stack.
//! - A block belongs to the organizing node with the greatest resolved
//!   `start_block` not after it; blocks before the first such node, or in
//!   a book with no TOC, attach directly under the root.
//! - Under any node, its direct leaves (in reading order) precede its
//!   child organizing nodes — a leaf can never follow a sibling sub-node
//!   in the source — which fixes a clean sibling ordinal order.

use std::collections::HashMap;
use std::fmt::Write as _;

use bookrack_core::NodeType;
use bookrack_corpus::{NewNode, NodeId};
use bookrack_extract::{BlockKind, Extraction};
use bookrack_normalize::{norm_text_sha256, normalize};
use sha2::{Digest, Sha256};

use bookrack_metadata::{FLAT_TOC_MIN_ENTRIES, HEADING_SKEW_MIN, HEADING_SKEW_RATIO, TocStats};

use crate::{IngestError, StructureParams};

/// The vec index of the book root within a plan. The root is always the
/// first planned node and is the only one with no parent.
const ROOT: usize = 0;

/// One node staged for insertion, with its parent held as a plan index.
///
/// Which fields are populated follows the node's group: organizing nodes
/// carry `title`; prose leaves carry the body text, statistics and
/// content hashes; structural leaves carry text and statistics but no
/// hashes. The document-order interval, page span and subtree signature
/// are computed after the list is built and stored alongside it.
struct PlannedNode {
    parent: Option<usize>,
    depth: i64,
    ordinal: i64,
    node_type: NodeType,
    title: Option<String>,
    text: Option<String>,
    char_count: Option<i64>,
    sentence_count: Option<i64>,
    /// Source page of a leaf — one block comes from one source unit.
    leaf_page: Option<i64>,
    /// Reading-order index of a leaf among all leaves.
    doc_order: Option<i64>,
    stable_anchor: Option<String>,
    text_sha256: Option<String>,
    norm_sha256: Option<String>,
}

impl PlannedNode {
    fn root(node_type: NodeType, title: Option<String>) -> PlannedNode {
        PlannedNode {
            parent: None,
            depth: 0,
            ordinal: 0,
            node_type,
            title,
            text: None,
            char_count: None,
            sentence_count: None,
            leaf_page: None,
            doc_order: None,
            stable_anchor: None,
            text_sha256: None,
            norm_sha256: None,
        }
    }

    fn organizing(parent: usize, depth: i64, node_type: NodeType, title: String) -> PlannedNode {
        PlannedNode {
            parent: Some(parent),
            depth,
            ordinal: 0,
            node_type,
            title: Some(title),
            text: None,
            char_count: None,
            sentence_count: None,
            leaf_page: None,
            doc_order: None,
            stable_anchor: None,
            text_sha256: None,
            norm_sha256: None,
        }
    }
}

/// A fully planned tree: the node list plus the per-node aggregates
/// computed over it. Indices into `nodes` are stable and shared by every
/// parallel vec.
pub(crate) struct TreePlan {
    nodes: Vec<PlannedNode>,
    doc_span: Vec<Option<(i64, i64)>>,
    page_span: Vec<Option<(i64, i64)>>,
    subtree_sig: Vec<Option<String>>,
    pub(crate) prose_leaves: usize,
}

impl TreePlan {
    /// How many node ids the corpus must allocate — every node but the
    /// root, whose id is the partition's reserved root offset.
    pub(crate) fn child_count(&self) -> u32 {
        (self.nodes.len() - 1) as u32
    }

    /// Bind plan indices to real node ids and emit insertable nodes.
    ///
    /// `ids` are the `child_count()` ids the allocator returned, in
    /// order; plan index `k >= 1` takes `ids[k - 1]`, index 0 is the
    /// `book_root_id`.
    pub(crate) fn into_new_nodes(self, book_root_id: NodeId, ids: &[NodeId]) -> Vec<NewNode> {
        let id_of = |idx: usize| -> NodeId {
            if idx == ROOT {
                book_root_id
            } else {
                ids[idx - 1]
            }
        };

        let mut out = Vec::with_capacity(self.nodes.len());
        for (idx, node) in self.nodes.iter().enumerate() {
            let mut nn = match node.parent {
                None => NewNode::root(book_root_id, node.node_type),
                Some(parent) => NewNode::child(
                    id_of(idx),
                    id_of(parent),
                    book_root_id,
                    node.ordinal,
                    node.depth,
                    node.node_type,
                ),
            };
            if node.node_type.is_organizing() {
                if let Some(title) = &node.title {
                    nn = nn.title(title.clone());
                }
                if let Some(sig) = &self.subtree_sig[idx] {
                    nn = nn.subtree_signature(sig.clone());
                }
            } else {
                if let Some(text) = &node.text {
                    nn = nn.text(text.clone());
                }
                nn = nn.text_stats(
                    node.char_count.unwrap_or(0),
                    node.sentence_count.unwrap_or(0),
                );
                if let (Some(anchor), Some(raw), Some(norm)) =
                    (&node.stable_anchor, &node.text_sha256, &node.norm_sha256)
                {
                    nn = nn.content_hashes(anchor.clone(), raw.clone(), norm.clone());
                }
            }
            if let Some((lo, hi)) = self.doc_span[idx] {
                nn = nn.toc_span(lo, hi);
            }
            if let Some((lo, hi)) = self.page_span[idx] {
                nn = nn.pages(lo, hi);
            }
            out.push(nn);
        }
        out
    }
}

/// Plan the node tree for one extraction.
///
/// Returns [`IngestError::EmptyExtraction`] if no block yields a prose
/// leaf — a book with no searchable body text is not an empty success.
pub(crate) fn plan_tree(
    book_root_type: NodeType,
    extraction: &Extraction,
    params: &StructureParams,
) -> Result<TreePlan, IngestError> {
    let blocks = &extraction.blocks;
    let entries = &extraction.toc.entries;

    // A heading block that re-states a TOC entry's label at that entry's
    // anchor is the chapter opener the organizing node already carries as
    // its title; drop the duplicate leaf.
    let mut labels_at: HashMap<usize, Vec<String>> = HashMap::new();
    for entry in entries {
        if let Some(start) = entry.start_block {
            labels_at
                .entry(start)
                .or_default()
                .push(normalize(&entry.label));
        }
    }
    let suppressed: Vec<bool> = blocks
        .iter()
        .enumerate()
        .map(|(i, block)| {
            matches!(block.kind, BlockKind::Heading { .. })
                && labels_at
                    .get(&i)
                    .is_some_and(|labels| labels.contains(&normalize(&block.text)))
        })
        .collect();

    // Owning organizing node per block: the entry with the greatest
    // resolved start_block not after the block, or the root (None).
    let mut resolved: Vec<(usize, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(t, entry)| entry.start_block.map(|start| (start, t)))
        .collect();
    resolved.sort_unstable();
    let mut owner_per_block: Vec<Option<usize>> = vec![None; blocks.len()];
    let mut ptr = 0;
    let mut current: Option<usize> = None;
    for (i, owner) in owner_per_block.iter_mut().enumerate() {
        while ptr < resolved.len() && resolved[ptr].0 <= i {
            current = Some(resolved[ptr].1);
            ptr += 1;
        }
        *owner = current;
    }

    // The organizing skeleton: parent and tree depth per TOC entry,
    // resolved with a depth stack over the flattened entry list.
    let num_orgs = entries.len();
    let org_index = |toc: usize| 1 + toc;
    let mut org_parent: Vec<usize> = vec![ROOT; num_orgs];
    let mut org_depth: Vec<i64> = vec![1; num_orgs];
    let mut stack: Vec<(usize, u8)> = Vec::new();
    for (t, entry) in entries.iter().enumerate() {
        while stack.last().is_some_and(|&(_, d)| d >= entry.depth) {
            stack.pop();
        }
        let (parent, parent_depth) = match stack.last() {
            Some(&(pt, _)) => (org_index(pt), org_depth[pt]),
            None => (ROOT, 0),
        };
        org_parent[t] = parent;
        org_depth[t] = parent_depth + 1;
        stack.push((t, entry.depth));
    }

    // Build the node list: root, then organizing nodes in TOC order, then
    // leaves in reading order.
    let mut nodes: Vec<PlannedNode> = Vec::with_capacity(1 + num_orgs + blocks.len());
    nodes.push(PlannedNode::root(
        book_root_type,
        extraction.biblio.title.clone(),
    ));
    for (t, entry) in entries.iter().enumerate() {
        nodes.push(PlannedNode::organizing(
            org_parent[t],
            org_depth[t],
            org_type(entry.depth),
            entry.label.clone(),
        ));
    }

    let mut prose_leaves = 0usize;
    let mut doc_order = 0i64;
    for (i, block) in blocks.iter().enumerate() {
        if suppressed[i] {
            continue;
        }
        let node_type = leaf_type(block.kind);
        let parent = match owner_per_block[i] {
            Some(t) => org_index(t),
            None => ROOT,
        };
        let depth = nodes[parent].depth + 1;
        let (anchor, raw, norm) = if node_type.is_prose_leaf() {
            let norm = norm_text_sha256(&block.text);
            let anchor = norm.chars().take(params.stable_anchor_len).collect();
            (
                Some(anchor),
                Some(sha256_hex(block.text.as_bytes())),
                Some(norm),
            )
        } else {
            (None, None, None)
        };
        if node_type.is_prose_leaf() {
            prose_leaves += 1;
        }
        nodes.push(PlannedNode {
            parent: Some(parent),
            depth,
            ordinal: 0,
            node_type,
            title: None,
            text: Some(block.text.clone()),
            char_count: Some(block.text.chars().count() as i64),
            sentence_count: Some(crate::sentences::count_sentences(&block.text)),
            leaf_page: Some(i64::from(block.source_unit)),
            doc_order: Some(doc_order),
            stable_anchor: anchor,
            text_sha256: raw,
            norm_sha256: norm,
        });
        doc_order += 1;
    }

    if prose_leaves == 0 {
        return Err(IngestError::EmptyExtraction);
    }

    // Sibling ordinals in document order: a parent's direct leaves (in
    // reading order, i.e. ascending leaf index) come first, then its
    // child organizing nodes (in TOC order).
    let n = nodes.len();
    let first_leaf = 1 + num_orgs;
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (offset, node) in nodes[first_leaf..].iter().enumerate() {
        if let Some(parent) = node.parent {
            children[parent].push(first_leaf + offset);
        }
    }
    for (offset, node) in nodes[1..first_leaf].iter().enumerate() {
        if let Some(parent) = node.parent {
            children[parent].push(1 + offset);
        }
    }
    for kids in &children {
        for (ord, &c) in kids.iter().enumerate() {
            nodes[c].ordinal = ord as i64;
        }
    }

    // Document-order and page intervals: a leaf spans itself; an
    // organizing node spans the union of its descendants. A parent always
    // has a smaller index than its children, so a reverse pass folds each
    // node's completed span into its parent.
    let mut doc_span: Vec<Option<(i64, i64)>> = vec![None; n];
    let mut page_span: Vec<Option<(i64, i64)>> = vec![None; n];
    for (idx, node) in nodes.iter().enumerate() {
        if let Some(d) = node.doc_order {
            doc_span[idx] = Some((d, d));
        }
        if let Some(p) = node.leaf_page {
            page_span[idx] = Some((p, p));
        }
    }
    for idx in (1..n).rev() {
        if let Some(parent) = nodes[idx].parent {
            doc_span[parent] = merge_span(doc_span[parent], doc_span[idx]);
            page_span[parent] = merge_span(page_span[parent], page_span[idx]);
        }
    }

    // Subtree content signature per organizing node: SHA-256 over the
    // in-order normalized-text hashes of its descendant prose leaves.
    let mut subtree_sig: Vec<Option<String>> = vec![None; n];
    subtree_hashes(ROOT, &nodes, &children, &mut subtree_sig);

    Ok(TreePlan {
        nodes,
        doc_span,
        page_span,
        subtree_sig,
        prose_leaves,
    })
}

/// Warning-level TOC shape statistics over one [`Extraction`].
///
/// `suspicious_flat` fires when a TOC has at least
/// [`FLAT_TOC_MIN_ENTRIES`] entries that all sit at one depth — enough
/// rows that a hierarchy could have been expressed, yet none was.
/// `heading_block_skew` fires when the TOC entry count and the body's
/// heading-block count diverge by at least [`HEADING_SKEW_RATIO`] in
/// either direction, once both sides clear [`HEADING_SKEW_MIN`].
pub(crate) fn toc_stats(extraction: &Extraction) -> TocStats {
    let entries = &extraction.toc.entries;
    let total = entries.len();
    let unanchored = entries.iter().filter(|e| e.start_block.is_none()).count();
    let suspicious_flat =
        total >= FLAT_TOC_MIN_ENTRIES && entries.iter().all(|e| e.depth == entries[0].depth);
    let heading_blocks = extraction
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Heading { .. }))
        .count();
    let heading_block_skew = (total >= HEADING_SKEW_MIN || heading_blocks >= HEADING_SKEW_MIN)
        && (total.saturating_mul(HEADING_SKEW_RATIO) < heading_blocks
            || heading_blocks.saturating_mul(HEADING_SKEW_RATIO) < total);
    TocStats {
        total_toc_entries: total,
        unanchored_toc_entries: unanchored,
        suspicious_flat,
        heading_block_skew,
    }
}

/// The organizing type for a TOC entry at `toc_depth` (0 is topmost).
fn org_type(toc_depth: u8) -> NodeType {
    match toc_depth {
        0 => NodeType::Chapter,
        1 => NodeType::Section,
        _ => NodeType::Subsection,
    }
}

/// The leaf type for a block. Captions become structural figure
/// captions; every other kind is a prose leaf.
fn leaf_type(kind: BlockKind) -> NodeType {
    match kind {
        BlockKind::Body | BlockKind::Other => NodeType::Paragraph,
        BlockKind::Heading { .. } => NodeType::Heading,
        BlockKind::Footnote => NodeType::Footnote,
        BlockKind::Caption => NodeType::FigureCaption,
    }
}

/// Union two optional inclusive intervals.
fn merge_span(a: Option<(i64, i64)>, b: Option<(i64, i64)>) -> Option<(i64, i64)> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some((alo, ahi)), Some((blo, bhi))) => Some((alo.min(blo), ahi.max(bhi))),
    }
}

/// Walk the subtree at `idx`, returning its descendant prose-leaf hashes
/// in document order and storing each organizing node's subtree
/// signature into `sig`.
fn subtree_hashes(
    idx: usize,
    nodes: &[PlannedNode],
    children: &[Vec<usize>],
    sig: &mut [Option<String>],
) -> Vec<String> {
    if !nodes[idx].node_type.is_organizing() {
        return match &nodes[idx].norm_sha256 {
            Some(hash) => vec![hash.clone()],
            None => Vec::new(),
        };
    }
    let mut hashes = Vec::new();
    for &child in &children[idx] {
        hashes.extend(subtree_hashes(child, nodes, children, sig));
    }
    let mut hasher = Sha256::new();
    for hash in &hashes {
        hasher.update(hash.as_bytes());
    }
    sig[idx] = Some(hex(hasher.finalize()));
    hashes
}

/// SHA-256 of `bytes`, as 64 lowercase hex characters.
fn sha256_hex(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

/// Render a digest as lowercase hex.
fn hex(digest: impl AsRef<[u8]>) -> String {
    let bytes = digest.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing formatted output into a String is infallible.
        write!(out, "{byte:02x}").expect("String write cannot fail");
    }
    out
}
