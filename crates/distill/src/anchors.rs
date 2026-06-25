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
    if s.len() > 80 {
        return false;
    }
    let first = s.chars().next().unwrap();
    // The headword line must lead with an uppercase ASCII letter.
    // Lower-case body lines (description prose) and digit-led runs
    // (years, page numbers) fall out here.
    if !first.is_ascii_uppercase() {
        return false;
    }
    if s.chars().any(is_cjk) {
        return false;
    }
    // Headwords are short. The four-word cap rules out
    // sentence-shaped body lines without enumerating every
    // punctuation form.
    if s.split_whitespace().count() > 4 {
        return false;
    }
    let has_lower = s.chars().any(|c| c.is_ascii_lowercase());
    let letter_count = s.chars().filter(|c| c.is_alphabetic()).count();
    // Reject all-caps multi-letter lines (running headers) but keep
    // single-letter entries like an alphabet header that the caller
    // elsewhere filters with `drop_lone_letter_dividers`.
    if !has_lower && letter_count > 2 {
        return false;
    }
    true
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
    fn latin_headword_rejects_running_headers_and_sentences() {
        assert!(!AnchorRule::LatinHeadword.matches("NEW YORK TIMES"));
        assert!(!AnchorRule::LatinHeadword.matches(
            "This is a complete sentence and ends with a period."
        ));
        assert!(!AnchorRule::LatinHeadword.matches("\u{53F2}\u{5BC6}\u{65AF}"));
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
        assert!(!rule.matches(
            "this rejected sentence is too long for either branch"
        ));
    }
}
