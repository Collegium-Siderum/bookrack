// SPDX-License-Identifier: Apache-2.0

//! Read passage text by structural position: a context window around
//! one content leaf, or an organizing node's span, paginated.
//!
//! Both reads are anchored on corpus node ids — the ids search
//! citations and TOC entries carry. A node id encodes its book's
//! partition, so neither read takes an intake id: the book is implied
//! and a forged pairing of book and node cannot be expressed.

use bookrack_core::{ItemKind, KindedNodeId, NodeId};
use bookrack_corpus::{Corpus, Node};
use bookrack_embed::Embedder;

use crate::dto::{
    ContextWindow, MAX_CONTEXT_RADIUS, MAX_READ_CHARS, MAX_SPAN_LEAVES, Passage, SpanText,
};
use crate::recorder::record_call_sync;
use crate::{Ops, OpsError, Result};

/// Read the leaves around one anchor leaf, in document order.
///
/// `before` / `after` count leaves on each side of the anchor and are
/// clamped to [`MAX_CONTEXT_RADIUS`]. When the window's text exceeds
/// [`MAX_READ_CHARS`], leaves are kept nearest-the-anchor first and
/// the window is marked truncated; the anchor itself is always
/// included, whatever its size.
///
/// Returns [`OpsError::NodeNotFound`] for an unknown node id and
/// [`OpsError::NotALeaf`] when the anchor is an organizing node or
/// carries no document-order position. Returns
/// [`OpsError::PapersBackendNotConfigured`] when the target is a
/// paper-kind node and this `Ops` was built without a papers backend.
pub fn read_context<E: Embedder>(
    ops: &Ops<E>,
    target: KindedNodeId,
    before: u32,
    after: u32,
) -> Result<ContextWindow> {
    let node_id = target.node_id.get();
    record_call_sync!(
        ops,
        "library.read_context",
        serde_json::json!({
            "kind": target.kind.as_scope_str(),
            "node_id": node_id,
            "before": before,
            "after": after,
        }),
        {
            let clamped = before > MAX_CONTEXT_RADIUS || after > MAX_CONTEXT_RADIUS;
            let before = i64::from(before.min(MAX_CONTEXT_RADIUS));
            let after = i64::from(after.min(MAX_CONTEXT_RADIUS));

            let corpus_path = match target.kind {
                ItemKind::Book => ops.corpus_db(),
                ItemKind::Paper => ops
                    .papers_corpus_db()
                    .ok_or(OpsError::PapersBackendNotConfigured)?,
            };
            let corpus = Corpus::open(corpus_path)?;
            let id = target.node_id;
            let Some(anchor) = corpus.get_node(id)? else {
                return Err(OpsError::NodeNotFound { node_id });
            };
            if anchor.node_type.is_organizing() {
                return Err(OpsError::NotALeaf { node_id });
            }
            let Some(pos) = anchor.toc_lo else {
                return Err(OpsError::NotALeaf { node_id });
            };

            let root = id.partition().root();
            let cap = usize::try_from(before + after + 1).unwrap_or(usize::MAX);
            let rows = corpus.leaves_in_doc_span(root, pos - before, pos + after, cap)?;
            let (passages, budget_dropped) = window_passages(&rows, id);
            Ok(ContextWindow {
                intake_id: id.partition().get(),
                anchor_node_id: node_id,
                passages,
                truncated: clamped || budget_dropped,
            })
        }
    )
}

/// Read one page of an organizing node's span, in document order.
///
/// The first call passes `start_after = None` and reads from the
/// span's start; each following call passes the previous response's
/// `next_offset` to resume. A page ends when [`MAX_READ_CHARS`] of
/// text (or [`MAX_SPAN_LEAVES`] rows) is reached; the first leaf of a
/// page is always included, whatever its size, so paging always makes
/// progress.
///
/// Returns [`OpsError::NodeNotFound`] for an unknown node id and
/// [`OpsError::NotOrganizing`] when the node is a leaf. Returns
/// [`OpsError::PapersBackendNotConfigured`] when the target is a
/// paper-kind node and this `Ops` was built without a papers backend.
pub fn read_span<E: Embedder>(
    ops: &Ops<E>,
    target: KindedNodeId,
    start_after: Option<i64>,
) -> Result<SpanText> {
    let node_id = target.node_id.get();
    record_call_sync!(
        ops,
        "library.read_span",
        serde_json::json!({
            "kind": target.kind.as_scope_str(),
            "node_id": node_id,
            "start_after": start_after,
        }),
        {
            let corpus_path = match target.kind {
                ItemKind::Book => ops.corpus_db(),
                ItemKind::Paper => ops
                    .papers_corpus_db()
                    .ok_or(OpsError::PapersBackendNotConfigured)?,
            };
            let corpus = Corpus::open(corpus_path)?;
            let id = target.node_id;
            let Some(node) = corpus.get_node(id)? else {
                return Err(OpsError::NodeNotFound { node_id });
            };
            if !node.node_type.is_organizing() {
                return Err(OpsError::NotOrganizing { node_id });
            }
            let intake_id = id.partition().get();
            let (Some(span_lo), Some(span_hi)) = (node.toc_lo, node.toc_hi) else {
                // An organizing node with no leaves has no span to read.
                return Ok(SpanText {
                    intake_id,
                    node_id,
                    title: node.title,
                    toc_lo: None,
                    toc_hi: None,
                    passages: Vec::new(),
                    next_offset: None,
                    truncated: false,
                });
            };

            let start = start_after
                .map_or(span_lo, |p| p.saturating_add(1))
                .max(span_lo);
            let root = id.partition().root();
            let rows = corpus.leaves_in_doc_span(root, start, span_hi, MAX_SPAN_LEAVES + 1)?;

            let mut passages = Vec::new();
            let mut budget = MAX_READ_CHARS as i64;
            let mut more = rows.len() > MAX_SPAN_LEAVES;
            for (i, row) in rows.iter().take(MAX_SPAN_LEAVES).enumerate() {
                let c = text_chars(row);
                if i > 0 && c > budget {
                    more = true;
                    break;
                }
                budget -= c;
                passages.push(Passage::from_node(row));
            }
            let next_offset = if more {
                passages.last().map(|p| p.toc_position)
            } else {
                None
            };
            Ok(SpanText {
                intake_id,
                node_id,
                title: node.title,
                toc_lo: Some(span_lo),
                toc_hi: Some(span_hi),
                passages,
                next_offset,
                truncated: next_offset.is_some(),
            })
        }
    )
}

/// Select the window rows that fit [`MAX_READ_CHARS`], expanding
/// outward from the anchor one ring at a time, and return them in
/// document order with a flag set when the budget dropped a fetched
/// row. Expansion on a side stops at the first leaf that does not
/// fit, so each side stays gapless.
fn window_passages(rows: &[Node], anchor_id: NodeId) -> (Vec<Passage>, bool) {
    let Some(anchor_idx) = rows.iter().position(|n| n.node_id == anchor_id) else {
        // The range query always returns the anchor's own row; its
        // absence means the corpus changed mid-read. Report an empty
        // window rather than inventing one without its anchor.
        return (Vec::new(), false);
    };
    let mut budget = MAX_READ_CHARS as i64 - text_chars(&rows[anchor_idx]);
    let mut lo = anchor_idx;
    let mut hi = anchor_idx;
    let mut lo_open = lo > 0;
    let mut hi_open = hi + 1 < rows.len();
    let mut dropped = false;
    while lo_open || hi_open {
        if lo_open {
            let c = text_chars(&rows[lo - 1]);
            if c <= budget {
                budget -= c;
                lo -= 1;
                lo_open = lo > 0;
            } else {
                lo_open = false;
                dropped = true;
            }
        }
        if hi_open {
            let c = text_chars(&rows[hi + 1]);
            if c <= budget {
                budget -= c;
                hi += 1;
                hi_open = hi + 1 < rows.len();
            } else {
                hi_open = false;
                dropped = true;
            }
        }
    }
    let passages = rows[lo..=hi].iter().map(Passage::from_node).collect();
    (passages, dropped)
}

/// Character count of one leaf's body text.
fn text_chars(node: &Node) -> i64 {
    node.text_content
        .as_deref()
        .map_or(0, |t| t.chars().count() as i64)
}
