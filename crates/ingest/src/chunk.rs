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
/// defaults, [`GROUP_SEPARATOR`], the version of `text-splitter` or of
/// any `icu_*` crate it segments with, or the trim setting. The chunk
/// text feeds `norm_chunk_sha256`, so a change is a re-embedding
/// commitment. The companion test `tests/dep_hash.rs` fails until
/// [`FROZEN_SPLITTER_DEPS`] is restated, forcing a deliberate bump.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const CHUNK_VERSION: u32 = 1;

/// `name@version` of `text-splitter` and every `icu_*` crate in
/// `Cargo.lock`, sorted bytewise. Restated in lockstep with
/// [`CHUNK_VERSION`]. See `tests/dep_hash.rs` for the selection rule.
pub const FROZEN_SPLITTER_DEPS: &[&str] = &[
    "icu_collections@2.2.0",
    "icu_locale@2.2.0",
    "icu_locale_core@2.2.0",
    "icu_locale_data@2.2.0",
    "icu_normalizer@2.2.0",
    "icu_normalizer_data@2.2.0",
    "icu_properties@2.2.0",
    "icu_properties_data@2.2.0",
    "icu_provider@2.2.0",
    "icu_segmenter@2.2.0",
    "icu_segmenter_data@2.2.0",
    "text-splitter@0.30.1",
];

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

    /// Configurations the golden fixture runs under: the production pair,
    /// then two small targets that cut every case many times, so a change
    /// in sentence, word, or grapheme segmentation moves a boundary.
    const GOLDEN_CONFIGS: &[(usize, usize)] = &[(1000, 100), (200, 50), (40, 10)];

    /// Digest of each case's planned chunks per configuration, recorded
    /// with the crates in `FROZEN_SPLITTER_DEPS`. A match says only that
    /// these inputs chunk as before; it does not show that a dependency
    /// move leaves every input's boundaries in place.
    const GOLDEN: &[(&str, &str)] = &[
        ("1000/100/lines-en", "efd6476d1f5aa099"),       // 3 chunks
        ("1000/100/para-zh", "79d7498ad336a93e"),        // 2 chunks
        ("1000/100/para-en", "2eb6ddc0f2b785d6"),        // 3 chunks
        ("1000/100/runon-zh", "aad7fc50cfcb8c5d"),       // 2 chunks
        ("1000/100/runon-en-colon", "db739837ec84eb18"), // 4 chunks
        ("1000/100/colon-chain", "795900d69f2f3a65"),    // 2 chunks
        ("1000/100/runon-th", "549d2459ed6e166a"),       // 7 chunks
        ("1000/100/mixed", "e130db0e96415c49"),          // 4 chunks
        ("1000/100/grapheme", "8931b6c73d9d43ff"),       // 2 chunks
        ("1000/100/parents", "cc9a206f70378dec"),        // 6 chunks
        ("1000/100/empty-leaves", "e6c59fba80fcb122"),   // 2 chunks
        ("1000/100/compat-forms", "0f071c5f2edeca5b"),   // 2 chunks
        ("200/50/lines-en", "845827ddba3e4c72"),         // 15 chunks
        ("200/50/para-zh", "a1a0c2ec4dbcf449"),          // 12 chunks
        ("200/50/para-en", "9ffb8e618527749b"),          // 14 chunks
        ("200/50/runon-zh", "147766dfb78abecf"),         // 12 chunks
        ("200/50/runon-en-colon", "4939fd5fc4ae2508"),   // 22 chunks
        ("200/50/colon-chain", "eda245331c9b0d8e"),      // 11 chunks
        ("200/50/runon-th", "77e1bec17a92211e"),         // 41 chunks
        ("200/50/mixed", "e047f1880cd5f844"),            // 19 chunks
        ("200/50/grapheme", "f21d17ceb9fbb8a5"),         // 10 chunks
        ("200/50/parents", "1f789de76edcb5f0"),          // 27 chunks
        ("200/50/empty-leaves", "523d275938095e1d"),     // 11 chunks
        ("200/50/compat-forms", "099506444d3f3c3e"),     // 11 chunks
        ("40/10/lines-en", "212292d10764545e"),          // 90 chunks
        ("40/10/para-zh", "8420f41085396366"),           // 60 chunks
        ("40/10/para-en", "e5d0c9e75ec57723"),           // 61 chunks
        ("40/10/runon-zh", "edcf48322162a505"),          // 60 chunks
        ("40/10/runon-en-colon", "15758cefee941a9e"),    // 105 chunks
        ("40/10/colon-chain", "3e318ba79acbf9c2"),       // 53 chunks
        ("40/10/runon-th", "a09d566d705fed06"),          // 207 chunks
        ("40/10/mixed", "e40ebb013ca89348"),             // 101 chunks
        ("40/10/grapheme", "f38a602d7476212a"),          // 47 chunks
        ("40/10/parents", "eb1388222a51c558"),           // 127 chunks
        ("40/10/empty-leaves", "dfc20cc96ea91bb5"),      // 47 chunks
        ("40/10/compat-forms", "9a93e443317316b7"),      // 51 chunks
    ];

    /// The fixture's cases as `(label, leaves)`. Leaf ids count up from 1
    /// across the file; each case and each `---` starts a new parent.
    fn golden_cases(fixture: &str) -> Vec<(&str, Vec<ChunkLeaf<'_>>)> {
        let mut cases: Vec<(&str, Vec<ChunkLeaf<'_>>)> = Vec::new();
        let (mut next_id, mut parent) = (1, 0);
        for line in fixture.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(label) = line.strip_prefix("=== ") {
                cases.push((label, Vec::new()));
                parent += 1;
            } else if line == "---" {
                parent += 1;
            } else {
                let text = if line == "<empty>" { "" } else { line };
                let case = cases.last_mut().expect("a leaf before the first case");
                case.1.push(leaf(next_id, parent, text));
                next_id += 1;
            }
        }
        cases
    }

    /// The text a plan's coordinates select: the covered part of every
    /// non-empty leaf from its start node to its end node, joined the way
    /// the group was.
    fn text_at(leaves: &[ChunkLeaf<'_>], plan: &ChunkPlan) -> String {
        let position = |id| leaves.iter().position(|l| l.node_id == id).unwrap();
        let (first, last) = (position(plan.start_node_id), position(plan.end_node_id));
        leaves[first..=last]
            .iter()
            .filter(|l| !l.text.is_empty())
            .map(|l| {
                let from = if l.node_id == plan.start_node_id {
                    plan.start_char_offset as usize
                } else {
                    0
                };
                let to = if l.node_id == plan.end_node_id {
                    plan.end_char_offset as usize
                } else {
                    l.text.chars().count()
                };
                l.text
                    .chars()
                    .skip(from)
                    .take(to - from)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(GROUP_SEPARATOR)
    }

    /// First 8 bytes, as hex, of a SHA-256 over every plan's coordinates,
    /// raw text, and normalized-text hash, in order.
    fn golden_digest(plans: &[ChunkPlan]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for p in plans {
            h.update(
                format!(
                    "{}:{}-{}:{}\n",
                    p.start_node_id, p.start_char_offset, p.end_node_id, p.end_char_offset
                )
                .as_bytes(),
            );
            h.update(p.text.as_bytes());
            h.update(b"\0");
            h.update(p.norm_chunk_sha256.as_bytes());
            h.update(b"\n");
        }
        h.finalize()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn chunk_boundaries_match_the_golden_digests() {
        let cases = golden_cases(include_str!("../tests/fixtures/chunk/golden.txt"));
        let mut actual = Vec::new();
        for &(target_chars, overlap_chars) in GOLDEN_CONFIGS {
            let params = ChunkParams {
                target_chars,
                overlap_chars,
            };
            for (label, leaves) in &cases {
                let key = format!("{target_chars}/{overlap_chars}/{label}");
                let plans = plan_chunks(leaves, &params);
                for (i, p) in plans.iter().enumerate() {
                    assert_eq!(
                        text_at(leaves, p),
                        p.text,
                        "{key}: chunk {i}'s coordinates do not select its text"
                    );
                }
                actual.push((key, golden_digest(&plans), plans.len()));
            }
        }
        let moved: Vec<&str> = actual
            .iter()
            .filter(|(k, d, _)| !GOLDEN.contains(&(k.as_str(), d.as_str())))
            .map(|(k, _, _)| k.as_str())
            .collect();
        let restated: String = actual
            .iter()
            .map(|(k, d, n)| format!("        (\"{k}\", \"{d}\"), // {n} chunks\n"))
            .collect();
        assert!(
            moved.is_empty() && actual.len() == GOLDEN.len(),
            "chunk boundaries moved for {moved:?}. A chunking dependency or setting changed \
             what plan_chunks produces: keep the previous dependency versions in Cargo.lock, or \
             bump CHUNK_VERSION and restate GOLDEN as\n{restated}"
        );
    }
}
