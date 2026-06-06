// SPDX-License-Identifier: Apache-2.0

//! Cross-language chapter / volume marker detection for plain-text
//! sources.
//!
//! Three independent templates cover the practical long-form-prose
//! corpus the TXT adapter sees today:
//!
//! - **Sino** — `<prefix><numeral><unit>` as one word. Covers
//!   Simplified and Traditional Chinese, Japanese (Aozora-style),
//!   and Sino-Korean conventions that route through the same shape.
//! - **Latin** — `<UnitWord> <Numeral>[ rest]` with the unit word
//!   followed by Roman, Arabic, or a small set of spelled-out first
//!   ordinals. Covers English, French, Spanish, and Italian as they
//!   appear in Project Gutenberg-style plain text.
//! - **German** — `<SpelledOrdinalAdj> <Unit>` as exactly two tokens.
//!   German novels of the Buddenbrooks era spell every ordinal
//!   (`Erstes Kapitel`, `F\u{00FC}nfzehntes Kapitel`); no Arabic
//!   chapter numbers appear, so the Latin template misses them
//!   entirely.
//!
//! The dispatcher tries the templates in source-cheapest order and
//! returns the first match. A miss in every template means the line
//! is body text. Heading level `1` is volume / part / book, level `2`
//! is chapter / section.
//!
//! Every pattern set comes from
//! [`bookrack_audit_profile::HeadingPatterns`], which is the
//! schema-locked shipped default plus an optional operator overlay
//! at `<data_root>/audit-rules/headings.toml`.

use bookrack_audit_profile::{GermanPatterns, HeadingPatterns, LatinPatterns, SinoPatterns};

/// Top-level dispatcher. Tries each template family in turn and
/// returns the heading level of the first match.
pub(crate) fn heading_level(line: &str, patterns: &HeadingPatterns) -> Option<u8> {
    sino::heading_level(line, &patterns.sino)
        .or_else(|| latin::heading_level(line, &patterns.latin))
        .or_else(|| german::heading_level(line, &patterns.german))
}

/// Sino template: `<prefix><numerals><unit>` written as one
/// uninterrupted word. The unit determines the level. Trailing content
/// after the unit (a title fragment, a page number) is permitted and
/// kept in the line label upstream.
mod sino {
    use super::SinoPatterns;

    pub(super) fn heading_level(line: &str, p: &SinoPatterns) -> Option<u8> {
        if line.chars().count() > p.max_chars {
            return None;
        }
        let prefix = p.prefix.chars().next()?;
        let mut chars = line.chars();
        if chars.next()? != prefix {
            return None;
        }
        let mut saw_number = false;
        for c in chars {
            if c.is_ascii_digit() || p.numerals.contains(c) {
                saw_number = true;
            } else if saw_number {
                return if p.volume_units.iter().any(|u| u.starts_with(c)) {
                    Some(1)
                } else if p.chapter_units.iter().any(|u| u.starts_with(c)) {
                    Some(2)
                } else {
                    None
                };
            } else {
                return None;
            }
        }
        None
    }
}

/// Latin template: a unit word followed by a Roman, Arabic, or
/// spelled-out first ordinal. The unit word is checked
/// case-insensitively so all-caps Italian-style headings still match.
mod latin {
    use super::LatinPatterns;

    pub(super) fn heading_level(line: &str, p: &LatinPatterns) -> Option<u8> {
        if line.chars().count() > p.max_chars {
            return None;
        }
        let mut words = line.split_whitespace();
        let unit_word = words.next()?;
        let unit_lower = unit_word.to_lowercase();
        let level = if p.volume_units.iter().any(|u| u == &unit_lower) {
            1
        } else if p.chapter_units.iter().any(|u| u == &unit_lower) {
            2
        } else {
            return None;
        };

        let numeral_raw = words.next()?;
        let numeral = strip_trailing_punctuation(numeral_raw);
        if numeral.is_empty() {
            return None;
        }

        if accepts_numeric(numeral, p) {
            return Some(level);
        }
        let numeral_lower = numeral.to_lowercase();
        if p.spelled_first.iter().any(|s| s == &numeral_lower) {
            return Some(level);
        }
        None
    }

    /// Accept the token when its longest prefix of Roman or Arabic
    /// numeral characters is at least one char long and whatever
    /// follows is either empty or a structural delimiter (`-`, `--`).
    /// This is what catches `Tome I--FANTINE` and `Chapter II`
    /// without dragging in the full Roman-numeral grammar.
    fn accepts_numeric(token: &str, p: &LatinPatterns) -> bool {
        let prefix_len: usize = token
            .chars()
            .take_while(|c| c.is_ascii_digit() || p.roman_chars.contains(*c))
            .map(char::len_utf8)
            .sum();
        if prefix_len == 0 || prefix_len > token.len() {
            return false;
        }
        let prefix = &token[..prefix_len];
        let rest = &token[prefix_len..];
        if prefix.chars().count() > p.roman_max_len && !prefix.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        rest.is_empty() || rest.starts_with('-')
    }

    fn strip_trailing_punctuation(s: &str) -> &str {
        s.trim_end_matches(['.', ':', ',', ';', '!', '?'])
    }
}

/// German template: an inflected ordinal adjective followed by the
/// unit word. Buddenbrooks-class novels carry no Arabic chapter
/// numbers at all; spelled ordinals are the only form. The two
/// supported endings — `-es` neuter for `Kapitel`, `-er` masculine
/// for `Teil` — are the gender forms the unit nouns take in nominative
/// case.
mod german {
    use super::GermanPatterns;

    pub(super) fn heading_level(line: &str, p: &GermanPatterns) -> Option<u8> {
        if line.chars().count() > p.max_chars {
            return None;
        }
        let mut words = line.split_whitespace();
        let ord = words.next()?.to_lowercase();
        let unit = words.next()?.to_lowercase();
        if words.next().is_some() {
            return None;
        }

        let (stem, level) = if let Some(s) = ord.strip_suffix("es") {
            if unit != "kapitel" {
                return None;
            }
            (s, 2u8)
        } else if let Some(s) = ord.strip_suffix("er") {
            if unit != "teil" {
                return None;
            }
            (s, 1u8)
        } else {
            return None;
        };

        if p.ordinal_stems.iter().any(|t| t == stem) {
            Some(level)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_audit_profile::HeadingPatterns;

    fn defaults() -> HeadingPatterns {
        HeadingPatterns::default_patterns()
    }

    fn level(line: &str) -> Option<u8> {
        heading_level(line, &defaults())
    }

    #[test]
    fn sino_chinese_chapter_with_numeral_returns_level_two() {
        // Synthetic CJK content via escapes: `\u{7B2C}\u{4E00}\u{7AE0}`
        // is the canonical first chapter heading.
        assert_eq!(level("\u{7B2C}\u{4E00}\u{7AE0}"), Some(2));
    }

    #[test]
    fn sino_chinese_volume_returns_level_one() {
        assert_eq!(level("\u{7B2C}\u{4E00}\u{5377}"), Some(1));
    }

    #[test]
    fn sino_japanese_volume_uses_jp_kanji() {
        // `\u{5DFB}` is the Japanese volume character. The dispatcher
        // routes it through the Sino template via the shared prefix.
        assert_eq!(level("\u{7B2C}\u{4E00}\u{5DFB}"), Some(1));
    }

    #[test]
    fn sino_japanese_episode_chapter_unit() {
        // `\u{8A71}` is the Japanese episode unit, common in serialised
        // novels carried by Aozora.
        assert_eq!(level("\u{7B2C}\u{4E09}\u{8A71}"), Some(2));
    }

    #[test]
    fn sino_with_arabic_numerals_works() {
        assert_eq!(level("\u{7B2C}5\u{7AE0}"), Some(2));
    }

    #[test]
    fn sino_with_trailing_title_text_still_matches() {
        // Real-world TXT often carries the chapter title and even a
        // page number on the same line as the marker.
        let line = "\u{7B2C}\u{4E00}\u{7AE0} \u{8BD5}\u{9A8C}\u{6587}\u{6848} 1";
        assert_eq!(level(line), Some(2));
    }

    #[test]
    fn latin_english_chapter_in_caps_with_roman_numeral() {
        assert_eq!(level("CHAPTER II."), Some(2));
    }

    #[test]
    fn latin_english_chapter_lowercase() {
        assert_eq!(level("Chapter XVIII."), Some(2));
    }

    #[test]
    fn latin_english_chapter_with_arabic_numeral() {
        assert_eq!(level("Chapter 14"), Some(2));
    }

    #[test]
    fn latin_french_with_inline_title() {
        assert_eq!(
            level("Chapitre V Que monseigneur Bienvenu faisait durer trop longtemps ses"),
            Some(2)
        );
    }

    #[test]
    fn latin_french_volume_marker_with_em_dash_title() {
        assert_eq!(level("Tome I--FANTINE"), Some(1));
    }

    #[test]
    fn latin_french_livre_with_spelled_first() {
        assert_eq!(level("Livre premier"), Some(1));
    }

    #[test]
    fn latin_spanish_with_spelled_first_and_inline_title() {
        assert_eq!(
            level(
                "Cap\u{00ED}tulo primero. Que trata de la condici\u{00F3}n y ejercicio del famoso"
            ),
            Some(2)
        );
    }

    #[test]
    fn latin_spanish_with_roman_numeral_and_inline_title() {
        assert_eq!(
            level("Cap\u{00ED}tulo II. Que trata de la primera salida que de su tierra"),
            Some(2)
        );
    }

    #[test]
    fn latin_italian_centered_uppercase_with_spelled_first() {
        // Italian Gutenberg novels heavily indent and uppercase
        // their chapter labels; the indent is stripped by the
        // caller before this fn sees the line.
        assert_eq!(level("CAPITOLO PRIMO."), Some(2));
    }

    #[test]
    fn latin_italian_with_roman_numeral_and_trailing_dot() {
        assert_eq!(level("CAPITOLO XXIII."), Some(2));
    }

    #[test]
    fn latin_rejects_ordinary_word_after_unit() {
        assert_eq!(level("Chapter introduces the protagonist"), None);
    }

    #[test]
    fn latin_rejects_unrelated_first_word() {
        assert_eq!(level("The Chapter II opens with"), None);
    }

    #[test]
    fn latin_rejects_a_line_longer_than_the_cap() {
        let mut line = String::from("Chapter II. ");
        line.push_str(&"a".repeat(120));
        assert_eq!(level(&line), None);
    }

    #[test]
    fn german_kapitel_with_spelled_ordinal() {
        assert_eq!(level("Erstes Kapitel"), Some(2));
    }

    #[test]
    fn german_teil_with_spelled_ordinal() {
        assert_eq!(level("Erster Teil"), Some(1));
    }

    #[test]
    fn german_ordinal_with_umlaut_stem() {
        assert_eq!(level("F\u{00FC}nfzehntes Kapitel"), Some(2));
    }

    #[test]
    fn german_lowercase_inputs_normalize_to_match() {
        assert_eq!(level("zweites kapitel"), Some(2));
    }

    #[test]
    fn german_wrong_gender_inflection_does_not_match() {
        // `-es` ending requires `Kapitel`, not `Teil`.
        assert_eq!(level("Erstes Teil"), None);
    }

    #[test]
    fn german_unknown_ordinal_stem_does_not_match() {
        assert_eq!(level("Phantastisches Kapitel"), None);
    }

    #[test]
    fn german_three_word_line_does_not_match() {
        assert_eq!(level("Erstes Kapitel von vielen"), None);
    }

    #[test]
    fn empty_and_whitespace_lines_return_none() {
        assert_eq!(level(""), None);
        assert_eq!(level("   "), None);
    }
}
