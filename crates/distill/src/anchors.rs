// SPDX-License-Identifier: Apache-2.0

//! Anchor rules used by the `walk_anchors` and
//! `walk_anchors_per_lang` stages.
//!
//! An [`AnchorRule`] is a small declarative predicate over a single
//! line of OCR Markdown. A rule's `matches` decides whether the line
//! marks the start of a new entry (i.e. is a headword). The
//! enumeration covers the four shapes the v1 books actually need plus
//! [`AnchorRule::AnyOf`] for hand-rolled combinations.

use crate::splitter::is_cjk;

/// A rule that decides whether one line of OCR Markdown is the start
/// of a new entry.
#[derive(Debug, Clone, PartialEq)]
pub enum AnchorRule {
    /// A typical latin-script headword: starts with a letter, mixed
    /// case (so all-caps running headers do not collide), no CJK,
    /// not a full sentence.
    LatinHeadword,

    /// A short CJK headword, bounded by `max_chars`. Used by
    /// bilingual books whose Chinese head is one short line.
    CjkShortHeadword { max_chars: usize },

    /// A short English headword bounded by `max_words`. Used by
    /// bilingual books whose English head is one short phrase.
    EnglishShortHeadword { max_words: usize },

    /// A running header — page number, short all-caps title, or
    /// similar. Used in the `reject` slot so headers do not look
    /// like anchors to the latin-headword rule.
    RejectRunningHeader,

    /// Disjunction: matches if any sub-rule matches.
    AnyOf(Vec<AnchorRule>),
}

impl AnchorRule {
    /// Does `line` (already trimmed by the caller, or trimmed here)
    /// satisfy this rule?
    pub fn matches(&self, line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        match self {
            AnchorRule::LatinHeadword => is_latin_headword(trimmed),
            AnchorRule::CjkShortHeadword { max_chars } => {
                is_cjk_short_headword(trimmed, *max_chars)
            }
            AnchorRule::EnglishShortHeadword { max_words } => {
                is_english_short_headword(trimmed, *max_words)
            }
            AnchorRule::RejectRunningHeader => is_running_header(trimmed),
            AnchorRule::AnyOf(rules) => rules.iter().any(|r| r.matches(trimmed)),
        }
    }
}

/// Per-language anchor configuration used by
/// `walk_anchors_per_lang` (§1.4 of the execution manual). One rule
/// is selected per block based on the block's `lang` tag.
#[derive(Debug, Clone)]
pub struct LangAnchorRule {
    pub lang: String,
    pub anchor: AnchorRule,
    pub reject: Vec<AnchorRule>,
    pub drop_lone_letter_dividers: bool,
    pub splice_orphans_to_prev_block: bool,
}

fn is_latin_headword(s: &str) -> bool {
    // Match a line that opens with `[uppercase latin][latin letter | ' | -]`.
    // Running-header / digit-only / CJK-only lines fall out because they
    // do not start with a latin uppercase letter; bilingual entries
    // that pack a latin headword, a CJK gloss, and a bracketed tag on
    // a single OCR row pass through unchanged. All-caps running
    // headers are filtered separately via the caller's
    // `reject = ["running_header"]` list.
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_latin_headword_first(first) {
        return false;
    }
    let Some(second) = chars.next() else {
        return false;
    };
    is_latin_headword_second(second)
}

fn is_latin_headword_first(c: char) -> bool {
    matches!(c,
        'A'..='Z'
        | '\u{00C0}'..='\u{00D6}'
        | '\u{00D8}'..='\u{00DE}'
        | '\u{0100}'..='\u{017F}'
    )
}

fn is_latin_headword_second(c: char) -> bool {
    matches!(c,
        'A'..='Z' | 'a'..='z'
        | '\u{00C0}'..='\u{00FF}'
        | '\u{0100}'..='\u{024F}'
        | '\u{1E00}'..='\u{1EFF}'
        | '\'' | '-'
    )
}

fn is_cjk_short_headword(s: &str, max_chars: usize) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() || chars.len() > max_chars {
        return false;
    }
    // All non-whitespace chars must be CJK.
    chars.iter().all(|c| c.is_whitespace() || is_cjk(*c))
}

fn is_english_short_headword(s: &str, max_words: usize) -> bool {
    if s.chars().any(is_cjk) {
        return false;
    }
    let first = s.chars().next().unwrap();
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.is_empty() || words.len() > max_words {
        return false;
    }
    // Reject sentence-shaped lines.
    let dots = s.chars().filter(|c| *c == '.').count();
    if dots > 1 {
        return false;
    }
    true
}

fn is_running_header(s: &str) -> bool {
    // Page numbers alone.
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    // Short all-caps title.
    let has_letter = s.chars().any(|c| c.is_alphabetic());
    if !has_letter {
        return false;
    }
    let letters_all_upper = s
        .chars()
        .filter(|c| c.is_alphabetic())
        .all(|c| c.is_uppercase());
    if letters_all_upper && s.len() < 40 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_headword_matches_typical_shapes() {
        assert!(AnchorRule::LatinHeadword.matches("Smith"));
        assert!(AnchorRule::LatinHeadword.matches("Smith, John"));
        assert!(AnchorRule::LatinHeadword.matches("Smith, J."));
    }

    #[test]
    fn latin_headword_accepts_mixed_latin_cjk_entries() {
        // OCR rows that pack the latin headword, its CJK gloss, and a
        // bracketed country tag onto one line are the dominant shape
        // in the name-translation dictionaries; the anchor must let
        // them through and leave splitting to `split_at_first_cjk`.
        assert!(
            AnchorRule::LatinHeadword
                .matches("Andra\u{0301}scsik \u{963F}\u{4F26}\u{5FB7} [\u{5308}]")
        );
        assert!(
            AnchorRule::LatinHeadword.matches(
                "Balch, Emily Greene (1867-1961) \u{5DF4}\u{5C14}\u{5947}\u{3008}\u{7F8E}\u{3009}\u{793E}\u{4F1A}\u{5B66}\u{5BB6}"
            )
        );
    }

    #[test]
    fn latin_headword_accepts_diacritic_initials() {
        assert!(AnchorRule::LatinHeadword.matches("\u{00D1}ervo"));
        assert!(AnchorRule::LatinHeadword.matches("\u{00C5}dahl"));
        assert!(AnchorRule::LatinHeadword.matches("\u{0160}imek"));
    }

    #[test]
    fn latin_headword_rejects_non_latin_initials() {
        assert!(!AnchorRule::LatinHeadword.matches("\u{53F2}\u{5BC6}\u{65AF}"));
        assert!(!AnchorRule::LatinHeadword.matches("1900-2000"));
        assert!(!AnchorRule::LatinHeadword.matches("an american baseball player"));
        assert!(!AnchorRule::LatinHeadword.matches(""));
        assert!(!AnchorRule::LatinHeadword.matches("A"));
    }

    #[test]
    fn running_header_rejection_layers_over_latin_headword() {
        // `latin_headword` itself no longer rules out all-caps
        // running headers or sentence-shaped prose; both still get
        // suppressed because callers wire `reject = ["running_header"]`
        // on the walk_anchors stage.
        let header = "NEW YORK TIMES";
        assert!(AnchorRule::LatinHeadword.matches(header));
        assert!(AnchorRule::RejectRunningHeader.matches(header));
    }

    #[test]
    fn cjk_short_headword_respects_max_chars() {
        let rule = AnchorRule::CjkShortHeadword { max_chars: 4 };
        assert!(rule.matches("\u{54F2}\u{5B66}"));
        assert!(!rule.matches("\u{54F2}\u{5B66}\u{77E5}\u{8BC6}\u{4E2D}"));
        assert!(!rule.matches("English"));
    }

    #[test]
    fn english_short_headword_caps_word_count() {
        let rule = AnchorRule::EnglishShortHeadword { max_words: 3 };
        assert!(rule.matches("philosophical knowledge"));
        assert!(!rule.matches("relating to philosophical knowledge widely"));
        assert!(!rule.matches("\u{54F2}\u{5B66}"));
    }

    #[test]
    fn running_header_matches_page_numbers_and_all_caps_titles() {
        assert!(AnchorRule::RejectRunningHeader.matches("42"));
        assert!(AnchorRule::RejectRunningHeader.matches("CHAPTER ONE"));
        assert!(!AnchorRule::RejectRunningHeader.matches("Smith"));
    }

    #[test]
    fn any_of_combines_rules() {
        let rule = AnchorRule::AnyOf(vec![
            AnchorRule::CjkShortHeadword { max_chars: 4 },
            AnchorRule::EnglishShortHeadword { max_words: 2 },
        ]);
        assert!(rule.matches("\u{54F2}\u{5B66}"));
        assert!(rule.matches("philosophical knowledge"));
        assert!(!rule.matches("this rejected sentence is too long for either branch"));
    }
}
