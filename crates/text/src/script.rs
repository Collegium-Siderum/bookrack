// SPDX-License-Identifier: Apache-2.0

//! Script predicates and bracket pairs, named by what they select.
//!
//! Every set here is frozen. Callers store or stamp what they decide
//! with a predicate, so a set that has to differ gets a new name
//! instead of an edit to an existing one; the test at the bottom of
//! this module pins each set and names what a change would move.

/// Whether `c` is a Han ideograph: CJK Unified Ideographs, Extension
/// A, the compatibility ideographs, and Extensions B through F.
///
/// Extract's quality scoring and PDF line joining decide with this
/// set, so a change to it moves `EXTRACTOR_VERSION`.
pub fn is_han(c: char) -> bool {
    matches!(
        u32::from(c),
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x2_0000..=0x2_A6DF
            | 0x2_A700..=0x2_EBEF
    )
}

/// Whether `c` is hiragana or katakana.
pub fn is_kana(c: char) -> bool {
    matches!(u32::from(c), 0x3040..=0x30FF)
}

/// Whether `c` sits in the Hangul Syllables block.
pub fn is_hangul_syllable(c: char) -> bool {
    matches!(u32::from(c), 0xAC00..=0xD7AF)
}

/// Whether `c` belongs to a CJK script: [`is_han`], [`is_kana`], or
/// [`is_hangul_syllable`].
pub fn is_cjk_script(c: char) -> bool {
    is_han(c) || is_kana(c) || is_hangul_syllable(c)
}

/// The fullwidth parentheses, opening then closing.
pub const FULLWIDTH_PARENS: (char, char) = ('\u{FF08}', '\u{FF09}');

/// The fullwidth square brackets, opening then closing.
pub const FULLWIDTH_SQUARE_BRACKETS: (char, char) = ('\u{FF3B}', '\u{FF3D}');

/// The lenticular brackets, opening then closing.
pub const LENTICULAR_BRACKETS: (char, char) = ('\u{3010}', '\u{3011}');

#[cfg(test)]
mod tests {
    use super::*;

    /// How many Unicode scalar values `predicate` takes.
    fn census(predicate: fn(char) -> bool) -> usize {
        ('\0'..=char::MAX).filter(|c| predicate(*c)).count()
    }

    /// Whether `predicate` takes both ends of every range and neither
    /// code point just outside them.
    fn holds_exactly_at_the_edges(predicate: fn(char) -> bool, ranges: &[(u32, u32)]) -> bool {
        let takes = |code: u32| char::from_u32(code).is_some_and(predicate);
        ranges
            .iter()
            .all(|&(lo, hi)| takes(lo) && takes(hi) && !takes(lo - 1) && !takes(hi + 1))
    }

    const HAN: &[(u32, u32)] = &[
        (0x3400, 0x4DBF),
        (0x4E00, 0x9FFF),
        (0xF900, 0xFAFF),
        (0x2_0000, 0x2_A6DF),
        (0x2_A700, 0x2_EBEF),
    ];
    const KANA: &[(u32, u32)] = &[(0x3040, 0x30FF)];
    const HANGUL: &[(u32, u32)] = &[(0xAC00, 0xD7AF)];

    #[test]
    fn the_han_set_is_frozen() {
        assert!(
            census(is_han) == 88_464 && holds_exactly_at_the_edges(is_han, HAN),
            "is_han changed. It feeds extract's quality scoring and PDF line joining, so the \
             change moves EXTRACTOR_VERSION; a set that differs on purpose gets a new name \
             instead."
        );
    }

    #[test]
    fn the_kana_and_hangul_sets_are_frozen() {
        assert!(
            census(is_kana) == 192 && holds_exactly_at_the_edges(is_kana, KANA),
            "is_kana changed; a set that differs on purpose gets a new name instead."
        );
        assert!(
            census(is_hangul_syllable) == 11_184
                && holds_exactly_at_the_edges(is_hangul_syllable, HANGUL),
            "is_hangul_syllable changed; a set that differs on purpose gets a new name instead."
        );
    }

    #[test]
    fn the_cjk_script_set_is_the_three_sets_and_nothing_else() {
        let ranges: Vec<(u32, u32)> = [HAN, KANA, HANGUL].concat();
        assert!(
            census(is_cjk_script) == 99_840 && holds_exactly_at_the_edges(is_cjk_script, &ranges),
            "is_cjk_script changed; a set that differs on purpose gets a new name instead."
        );
    }

    #[test]
    fn the_bracket_pairs_open_then_close() {
        assert_eq!(FULLWIDTH_PARENS, ('\u{FF08}', '\u{FF09}'));
        assert_eq!(FULLWIDTH_SQUARE_BRACKETS, ('\u{FF3B}', '\u{FF3D}'));
        assert_eq!(LENTICULAR_BRACKETS, ('\u{3010}', '\u{3011}'));
    }
}
