// SPDX-License-Identifier: Apache-2.0

//! Slicing a unit's leaves into segments: which leaf types become
//! segments, how long a segment may grow before it is cut at a
//! sentence boundary, and where those boundaries are.
//!
//! Everything here is a pure function over text and node types; the
//! plan write that uses it, and the per-script defaults for the length
//! limit, live behind the MCP tool. The limit is an escape threshold,
//! not a slicing step: a leaf within it stays one segment, a leaf
//! beyond it is cut at sentence boundaries, and a single sentence
//! longer than the limit is kept whole rather than cut mid-sentence.

use bookrack_core::NodeType;

/// What a plan does with a leaf of a given type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Triage {
    /// Becomes one or more segments, cut at sentence boundaries past
    /// the threshold.
    Segment,
    /// Becomes exactly one segment covering the whole leaf, never cut:
    /// the line structure of a quotation or poem is part of its text.
    SegmentWhole,
    /// Stays in the book untranslated; reported, not planned.
    Skip {
        /// Why the leaf is left as it is.
        reason: &'static str,
        /// Whether a person should look at the leaf anyway.
        needs_attention: bool,
    },
    /// Not part of the text at all; neither planned nor reported as
    /// skipped work.
    Exclude(&'static str),
}

/// The plan's verdict for each node type. Exhaustive on purpose: a
/// node type added to the corpus fails to compile here rather than
/// falling into a default arm.
pub const fn triage(node_type: NodeType) -> Triage {
    match node_type {
        NodeType::Paragraph | NodeType::Heading | NodeType::Footnote | NodeType::FigureCaption => {
            Triage::Segment
        }
        NodeType::Quote | NodeType::Poem => Triage::SegmentWhole,
        NodeType::Formula | NodeType::Code => Triage::Skip {
            reason: "not translatable",
            needs_attention: false,
        },
        NodeType::Table => Triage::Skip {
            reason: "table cells are beyond the segment model",
            needs_attention: true,
        },
        NodeType::Figure => Triage::Exclude("a figure carries no text"),
        NodeType::RunningHeader => Triage::Exclude("page furniture"),
        NodeType::ImageGarbage => Triage::Exclude("extraction noise"),
        NodeType::Collection
        | NodeType::Volume
        | NodeType::Work
        | NodeType::Chapter
        | NodeType::Section
        | NodeType::Subsection => Triage::Exclude("organizing node"),
    }
}

/// The script family a text is written in, as far as the caller's
/// choice of escape threshold cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Latin,
    Cjk,
}

/// Decide the script of `text` by its alphanumeric chars: CJK when
/// more than half of them sit at or above U+2E80, Latin otherwise and
/// for a text with no alphanumeric chars at all.
pub fn script_of(text: &str) -> Script {
    let (mut alnum, mut cjk) = (0usize, 0usize);
    for c in text.chars().filter(|c| c.is_alphanumeric()) {
        alnum += 1;
        if (c as u32) >= 0x2E80 {
            cjk += 1;
        }
    }
    if cjk * 2 > alnum {
        Script::Cjk
    } else {
        Script::Latin
    }
}

/// Whether `c` ends a sentence. The same nine terminators the corpus
/// sentence count recognises: the Latin set and the full-width and
/// ideographic forms a CJK text uses.
fn is_terminator(c: char) -> bool {
    matches!(
        c,
        '.' | '!'
            | '?'
            | ';'
            | '\u{3002}' // ideographic full stop
            | '\u{FF01}' // fullwidth exclamation mark
            | '\u{FF1F}' // fullwidth question mark
            | '\u{FF1B}' // fullwidth semicolon
            | '\u{2026}' // horizontal ellipsis
    )
}

/// Whether `c` may trail a terminator and still belong to the sentence
/// it closes: closing quotation marks and brackets.
fn is_closer(c: char) -> bool {
    matches!(
        c,
        '"' | '\''
            | ')'
            | ']'
            | '\u{BB}' // right-pointing double angle quotation mark
            | '\u{2019}' // right single quotation mark
            | '\u{201D}' // right double quotation mark
            | '\u{300D}' // right corner bracket
            | '\u{300F}' // right white corner bracket
            | '\u{FF09}' // fullwidth right parenthesis
    )
}

/// Char offsets at which a new sentence starts: after a run of
/// terminators and whatever closers and whitespace trail it. Strictly
/// increasing, never 0 and never the text's length.
pub fn sentence_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries = Vec::new();
    let mut after_terminator = false;
    for (i, c) in text.chars().enumerate() {
        if is_terminator(c) {
            after_terminator = true;
        } else if after_terminator {
            if is_closer(c) || c.is_whitespace() {
                continue;
            }
            boundaries.push(i);
            after_terminator = false;
        }
    }
    boundaries
}

/// Cut `text` into half-open char ranges that chain from 0 to its
/// length. A text within `max_chars` is one range. Past the threshold,
/// each range ends at the last sentence boundary within the threshold
/// from its start; when no boundary lies within it, at the first one
/// beyond, so a long sentence stays whole; with no boundary left, the
/// remainder is one range. Empty text yields no ranges.
pub fn split_at_sentences(text: &str, max_chars: usize) -> Vec<(usize, usize)> {
    let len = text.chars().count();
    if len == 0 {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);
    let boundaries = sentence_boundaries(text);
    let mut ranges = Vec::new();
    let mut start = 0;
    while len - start > max_chars {
        let window_end = start + max_chars;
        let within = boundaries
            .iter()
            .copied()
            .rfind(|&b| b > start && b <= window_end);
        let cut = within.or_else(|| boundaries.iter().copied().find(|&b| b > window_end));
        match cut {
            Some(end) => {
                ranges.push((start, end));
                start = end;
            }
            None => break,
        }
    }
    ranges.push((start, len));
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_node_type_has_a_verdict_and_the_groups_have_the_expected_sizes() {
        let mut segment = 0;
        let mut whole = 0;
        let mut skip = 0;
        let mut attention = 0;
        let mut exclude = 0;
        for t in NodeType::ALL {
            match triage(t) {
                Triage::Segment => segment += 1,
                Triage::SegmentWhole => whole += 1,
                Triage::Skip {
                    needs_attention, ..
                } => {
                    skip += 1;
                    if needs_attention {
                        attention += 1;
                    }
                }
                Triage::Exclude(_) => exclude += 1,
            }
            if t.is_organizing() {
                assert!(matches!(triage(t), Triage::Exclude(_)), "{t:?}");
            }
        }
        assert_eq!((segment, whole, skip, attention, exclude), (4, 2, 3, 1, 9));
        assert_eq!(triage(NodeType::Poem), Triage::SegmentWhole);
        assert!(matches!(
            triage(NodeType::Table),
            Triage::Skip {
                needs_attention: true,
                ..
            }
        ));
    }

    #[test]
    fn script_is_decided_by_the_majority_of_alphanumeric_chars() {
        assert_eq!(
            script_of("Als Gregor Samsa eines Morgens erwachte"),
            Script::Latin
        );
        assert_eq!(
            script_of(
                "\u{4e00}\u{5929}\u{65e9}\u{6668}\u{ff0c}\u{683c}\u{91cc}\u{9ad8}\u{5c14}\u{9192}\u{4e86} Gregor"
            ),
            Script::Cjk
        );
        assert_eq!(script_of("Gregor \u{9192}"), Script::Latin);
        assert_eq!(
            script_of("\u{4e00}\u{5929}\u{65e9}\u{6668} Gregor \u{9192}\u{4e86}"),
            Script::Latin,
            "an even split is not a CJK majority"
        );
        assert_eq!(script_of("... !!! 123"), Script::Latin);
    }

    #[test]
    fn boundaries_follow_terminator_runs_closers_and_whitespace() {
        assert_eq!(sentence_boundaries("One. Two! Three?"), vec![5, 10]);
        assert_eq!(sentence_boundaries("Wait... what?! Yes."), vec![8, 15]);
        assert_eq!(sentence_boundaries("He said \"go.\" She went."), vec![14]);
        assert_eq!(
            sentence_boundaries("\u{4ed6}\u{8bf4}\u{3002}\u{300d}\u{5979}\u{8d70}\u{4e86}\u{3002}"),
            vec![4]
        );
        assert_eq!(
            sentence_boundaries("no terminator here"),
            Vec::<usize>::new()
        );
        assert_eq!(sentence_boundaries("ends here."), Vec::<usize>::new());
    }

    fn chained(ranges: &[(usize, usize)], len: usize) -> bool {
        ranges.first().is_some_and(|r| r.0 == 0)
            && ranges.last().is_some_and(|r| r.1 == len)
            && ranges.windows(2).all(|w| w[0].1 == w[1].0)
            && ranges.iter().all(|r| r.0 < r.1)
    }

    #[test]
    fn a_text_within_the_threshold_is_one_range_and_empty_text_is_none() {
        assert_eq!(split_at_sentences("One. Two.", 100), vec![(0, 9)]);
        assert_eq!(split_at_sentences("", 100), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn a_long_text_is_cut_at_the_last_boundary_within_the_threshold() {
        let text = "One two. Three four. Five six. Seven.";
        assert_eq!(sentence_boundaries(text), vec![9, 21, 31]);
        let ranges = split_at_sentences(text, 20);
        assert_eq!(ranges, vec![(0, 9), (9, 21), (21, 37)]);
        assert!(chained(&ranges, text.chars().count()));
        let ranges = split_at_sentences(text, 25);
        assert_eq!(
            ranges,
            vec![(0, 21), (21, 37)],
            "two boundaries inside the window: the last one is the cut"
        );
    }

    #[test]
    fn a_sentence_longer_than_the_threshold_is_kept_whole() {
        let text = "Short. A very long sentence that runs past the threshold without a stop. End.";
        let ranges = split_at_sentences(text, 10);
        assert_eq!(ranges, vec![(0, 7), (7, 73), (73, 77)]);
        assert!(chained(&ranges, text.chars().count()));

        let single = "One long sentence with no terminator at all";
        assert_eq!(
            split_at_sentences(single, 5),
            vec![(0, single.chars().count())]
        );
    }

    #[test]
    fn cjk_text_is_cut_at_fullwidth_terminators_by_char_not_byte() {
        let text =
            "\u{4e00}\u{4e8c}\u{4e09}\u{3002}\u{56db}\u{4e94}\u{516d}\u{ff01}\u{4e03}\u{516b}";
        let ranges = split_at_sentences(text, 5);
        assert_eq!(ranges, vec![(0, 4), (4, 8), (8, 10)]);
        assert!(chained(&ranges, 10));
    }

    #[test]
    fn every_split_chains_from_zero_to_the_length() {
        let samples = [
            "A. B. C. D. E. F.",
            "Alpha beta gamma. Delta? Epsilon! Zeta... Eta.",
            "\u{4ed6}\u{8bf4}\u{3002}\u{5979}\u{8d70}\u{4e86}\u{ff01}\u{4ed6}\u{4e5f}\u{8d70}\u{4e86}\u{3002}",
            "No stops anywhere in this one at all",
            "x",
        ];
        for text in samples {
            for max in [1, 3, 7, 20, 1000] {
                let ranges = split_at_sentences(text, max);
                assert!(
                    chained(&ranges, text.chars().count()),
                    "{text:?} @ {max}: {ranges:?}"
                );
            }
        }
    }
}
