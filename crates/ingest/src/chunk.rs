// SPDX-License-Identifier: Apache-2.0

//! Chunking: group a book's prose leaves and split them into the
//! retrieval units that get embedded.
//!
//! A prose leaf is usually one short paragraph — far below a useful
//! embedding size. Chunking first groups consecutive leaves that share a
//! parent (so a chunk never crosses an organizing boundary), concatenates
//! each group in document order, and splits the concatenation into
//! overlapping windows of a target character length. Each window records
//! the node range it covers, so a hit maps back to a precise source span.
//!
//! [`plan_chunks`] is pure and deterministic: the same leaves and the same
//! [`ChunkParams`] yield byte-identical chunks, which is what lets
//! `norm_chunk_sha256` serve as a stable dedup and cache key. Any change
//! that moves a chunk boundary — target length, overlap, the join
//! separator, the splitter version, or trimming — changes that identity
//! and so must bump [`CHUNK_VERSION`].

use bookrack_core::NodeId;
use bookrack_normalize::norm_text_sha256;
use text_splitter::{ChunkConfig, TextSplitter};

/// Version of the chunking behaviour.
///
/// Bump on any change that moves a chunk boundary: [`ChunkParams`]
/// defaults, [`GROUP_SEPARATOR`], the `text-splitter` version, or the
/// trim setting. The chunk text feeds `norm_chunk_sha256`, so a change is
/// a re-embedding commitment.
pub const CHUNK_VERSION: u32 = 1;

/// String joining adjacent leaves within one group before splitting. Part
/// of chunk identity, hence covered by [`CHUNK_VERSION`].
const GROUP_SEPARATOR: &str = "\n\n";

/// Tuning parameters for chunking. The defaults are the frozen, versioned
/// values; overriding them is for experiments and tests, not a runtime
/// knob — a different value is a different [`CHUNK_VERSION`].
#[derive(Debug, Clone)]
pub struct ChunkParams {
    /// Target chunk length, in Unicode scalar values.
    pub target_chars: usize,
    /// Overlap between adjacent chunks of one group, in characters.
    pub overlap_chars: usize,
}

impl Default for ChunkParams {
    fn default() -> ChunkParams {
        ChunkParams {
            target_chars: 1000,
            overlap_chars: 100,
        }
    }
}

/// One planned chunk: the text to embed and the source span it covers.
/// Mirrors the non-vector columns of a vector-store row, so embedding can
/// turn it straight into a stored chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPlan {
    /// Leaf the chunk starts in.
    pub start_node_id: NodeId,
    /// Character offset of the chunk's start within `start_node_id`.
    pub start_char_offset: i32,
    /// Leaf the chunk ends in.
    pub end_node_id: NodeId,
    /// Character offset of the chunk's end within `end_node_id`.
    pub end_char_offset: i32,
    /// The chunk text, joined and trimmed.
    pub text: String,
    /// SHA-256 of the normalized chunk text; the dedup and cache key.
    pub norm_chunk_sha256: String,
}

/// One prose leaf, in document order, as input to [`plan_chunks`].
pub(crate) struct ChunkLeaf<'a> {
    pub node_id: NodeId,
    pub parent_id: Option<NodeId>,
    pub text: &'a str,
}

/// A leaf's text and where it sits in the group concatenation.
struct Segment<'a> {
    node_id: NodeId,
    byte_start: usize,
    text: &'a str,
    char_len: i32,
}

/// Which way to snap a concatenation byte position that falls in a
/// separator gap: forward to the next leaf, or back to the previous one.
enum Snap {
    Start,
    End,
}

/// Plan the chunks for one book's prose leaves, already in document order.
///
/// Leaves are grouped into maximal runs sharing a parent; each group is
/// concatenated and split independently, so no chunk spans two organizing
/// nodes. Empty leaves and empty groups produce no chunks.
pub(crate) fn plan_chunks(leaves: &[ChunkLeaf<'_>], params: &ChunkParams) -> Vec<ChunkPlan> {
    let config = ChunkConfig::new(params.target_chars)
        .with_overlap(params.overlap_chars)
        .expect("chunk overlap must be smaller than the target length");
    let splitter = TextSplitter::new(config);

    let mut plans = Vec::new();
    for group in group_by_parent(leaves) {
        // Concatenate the group's leaves, remembering where each leaf's
        // text begins so a chunk's byte span maps back to source nodes.
        let mut concat = String::new();
        let mut segments = Vec::with_capacity(group.len());
        for leaf in group {
            if leaf.text.is_empty() {
                continue;
            }
            if !concat.is_empty() {
                concat.push_str(GROUP_SEPARATOR);
            }
            let byte_start = concat.len();
            concat.push_str(leaf.text);
            segments.push(Segment {
                node_id: leaf.node_id,
                byte_start,
                text: leaf.text,
                char_len: leaf.text.chars().count() as i32,
            });
        }
        if segments.is_empty() {
            continue;
        }

        for (byte_start, chunk) in splitter.chunk_indices(&concat) {
            if chunk.is_empty() {
                continue;
            }
            let byte_end = byte_start + chunk.len();
            let (start_node_id, start_char_offset) = locate(&segments, byte_start, Snap::Start);
            let (end_node_id, end_char_offset) = locate(&segments, byte_end, Snap::End);
            plans.push(ChunkPlan {
                start_node_id,
                start_char_offset,
                end_node_id,
                end_char_offset,
                norm_chunk_sha256: norm_text_sha256(chunk),
                text: chunk.to_string(),
            });
        }
    }
    plans
}

/// Group leaves into maximal runs that share a `parent_id`, preserving
/// document order. Same-parent leaves are contiguous in document order
/// (STRUCTURE places a parent's direct leaves before its child organizing
/// nodes), so a run break marks a real organizing boundary.
fn group_by_parent<'a, 'b>(leaves: &'b [ChunkLeaf<'a>]) -> Vec<&'b [ChunkLeaf<'a>]> {
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..leaves.len() {
        if leaves[i].parent_id != leaves[start].parent_id {
            groups.push(&leaves[start..i]);
            start = i;
        }
    }
    if !leaves.is_empty() {
        groups.push(&leaves[start..]);
    }
    groups
}

/// Map a byte position in the group concatenation to a `(node, char
/// offset)` pair. A position inside a separator gap snaps forward to the
/// next leaf for a chunk start, or back to the previous leaf for a chunk
/// end, so a chunk's span always lands on real leaf text.
fn locate(segments: &[Segment<'_>], byte_pos: usize, snap: Snap) -> (NodeId, i32) {
    let mut prev: Option<&Segment> = None;
    for seg in segments {
        let seg_start = seg.byte_start;
        let seg_end = seg.byte_start + seg.text.len();
        if byte_pos < seg_start {
            return match snap {
                Snap::Start => (seg.node_id, 0),
                Snap::End => match prev {
                    Some(p) => (p.node_id, p.char_len),
                    None => (seg.node_id, 0),
                },
            };
        }
        let within = match snap {
            Snap::Start => byte_pos < seg_end,
            Snap::End => byte_pos <= seg_end,
        };
        if within {
            let rel = byte_pos - seg_start;
            let offset = seg.text[..rel].chars().count() as i32;
            return (seg.node_id, offset);
        }
        prev = Some(seg);
    }
    let last = segments.last().expect("a group has at least one segment");
    (last.node_id, last.char_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: i64, parent: i64, text: &str) -> ChunkLeaf<'_> {
        ChunkLeaf {
            node_id: NodeId::new(id),
            parent_id: Some(NodeId::new(parent)),
            text,
        }
    }

    #[test]
    fn no_leaves_make_no_chunks() {
        assert!(plan_chunks(&[], &ChunkParams::default()).is_empty());
    }

    #[test]
    fn a_short_leaf_becomes_one_chunk_spanning_itself() {
        let leaves = vec![leaf(10, 1, "Hello, world.")];
        let plans = plan_chunks(&leaves, &ChunkParams::default());
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert_eq!(p.start_node_id, NodeId::new(10));
        assert_eq!(p.end_node_id, NodeId::new(10));
        assert_eq!(p.start_char_offset, 0);
        assert_eq!(p.end_char_offset, "Hello, world.".chars().count() as i32);
        assert_eq!(p.text, "Hello, world.");
        assert_eq!(p.norm_chunk_sha256, norm_text_sha256("Hello, world."));
    }

    #[test]
    fn chunking_is_deterministic() {
        let leaves = vec![
            leaf(10, 1, "First paragraph with some words."),
            leaf(11, 1, "Second paragraph follows here."),
        ];
        let a = plan_chunks(&leaves, &ChunkParams::default());
        let b = plan_chunks(&leaves, &ChunkParams::default());
        assert_eq!(a, b);
    }

    #[test]
    fn small_leaves_merge_into_one_chunk() {
        // Many tiny same-parent leaves merge instead of one-chunk-per-leaf.
        let leaves: Vec<ChunkLeaf> = (0..20).map(|i| leaf(10 + i, 1, "short")).collect();
        let plans = plan_chunks(&leaves, &ChunkParams::default());
        assert_eq!(plans.len(), 1, "20 tiny leaves should merge into one chunk");
        let p = &plans[0];
        assert_eq!(p.start_node_id, NodeId::new(10));
        assert_eq!(p.end_node_id, NodeId::new(29));
    }

    #[test]
    fn a_chunk_never_spans_two_parents() {
        // Group A is all 'a', group B is all 'b'; no chunk mixes them.
        let leaves = vec![
            leaf(10, 1, "aaaa"),
            leaf(11, 1, "aaaa"),
            leaf(20, 2, "bbbb"),
            leaf(21, 2, "bbbb"),
        ];
        let plans = plan_chunks(&leaves, &ChunkParams::default());
        for p in &plans {
            let has_a = p.text.contains('a');
            let has_b = p.text.contains('b');
            assert!(
                !(has_a && has_b),
                "chunk crossed a parent boundary: {:?}",
                p.text
            );
        }
        assert!(
            plans.len() >= 2,
            "two parents must yield at least two chunks"
        );
    }

    #[test]
    fn a_long_leaf_splits_within_the_same_node() {
        let params = ChunkParams {
            target_chars: 20,
            overlap_chars: 5,
        };
        let long = "word ".repeat(40); // 200 chars in one leaf
        let leaves = vec![leaf(10, 1, &long)];
        let plans = plan_chunks(&leaves, &params);
        assert!(plans.len() > 1, "a long leaf should split");
        for p in &plans {
            assert_eq!(p.start_node_id, NodeId::new(10));
            assert_eq!(p.end_node_id, NodeId::new(10));
        }
    }

    #[test]
    fn single_node_offsets_reconstruct_the_chunk() {
        // For chunks confined to one node, slicing that node's text by the
        // recorded char offsets must reproduce the chunk text exactly.
        let text = "abcdefghij";
        let params = ChunkParams {
            target_chars: 4,
            overlap_chars: 0,
        };
        let leaves = vec![leaf(10, 1, text)];
        let plans = plan_chunks(&leaves, &params);
        let chars: Vec<char> = text.chars().collect();
        assert!(!plans.is_empty());
        for p in &plans {
            assert_eq!(p.start_node_id, NodeId::new(10));
            assert_eq!(p.end_node_id, NodeId::new(10));
            let slice: String = chars[p.start_char_offset as usize..p.end_char_offset as usize]
                .iter()
                .collect();
            assert_eq!(slice, p.text);
        }
    }
}
