// SPDX-License-Identifier: Apache-2.0

//! Frozen, deterministic text normalization.
//!
//! [`normalize`] strips surface noise — Unicode form, whitespace,
//! punctuation encoding, line endings — that varies between extractors
//! and editions of otherwise-identical prose. Its output is the input
//! to [`norm_text_sha256`], the content identity used to recognize that
//! two passages are "the same text" across different files.
//!
//! This is a frozen invariant. The exact byte output of [`normalize`]
//! must not change once any content has been ingested: a change would
//! alter every stored content hash and silently break cross-file
//! deduplication. Any change to the algorithm therefore requires
//! bumping [`NORMALIZE_VERSION`] and rebuilding the whole corpus.
//!
//! The transformation is a fixed nine-step pipeline. The order is part
//! of the contract and must not be rearranged:
//!
//! 1. Line endings: CRLF, CR, vertical tab, form feed, and the
//!    Unicode next-line / line / paragraph separators -> LF.
//! 2. Unicode NFKC normalization.
//! 3. Strip zero-width and otherwise invisible characters.
//! 4. Unify punctuation encodings (dashes, ellipsis, curly quotes).
//! 5. Convert horizontal whitespace (tab) to a plain space.
//! 6. Per line: trim, then collapse runs of spaces to one.
//! 7. Delete a lone space flanked by CJK characters on both sides.
//! 8. Collapse runs of three or more newlines to two.
//! 9. Trim leading and trailing whitespace from the whole string.
//!
//! The function never touches glyphs: no case folding, and no Han
//! traditional/simplified conversion — a traditional edition and a
//! simplified edition are deliberately distinct content.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// Version of the normalization algorithm.
///
/// Bump on any change to [`normalize`]'s behaviour. The value is
/// recorded in the index so a daemon can refuse to serve an index that
/// was built with a different algorithm version. Bumping it is a
/// commitment to rebuild the whole corpus.
pub const NORMALIZE_VERSION: u32 = 1;

/// Normalize prose text to its frozen canonical form.
///
/// Pure and deterministic: the same input always yields the same
/// output, with no dependence on configuration, environment or locale.
/// Empty or whitespace-only input yields an empty string.
pub fn normalize(input: &str) -> String {
    let s = normalize_line_endings(input);
    let s: String = s.nfkc().collect();
    let s = strip_invisibles(&s);
    let s = unify_punctuation(&s);
    let s = horizontal_whitespace_to_space(&s);
    let s = fold_lines(&s);
    let s = delete_cjk_flanked_space(&s);
    let s = fold_blank_lines(&s);
    s.trim().to_string()
}

/// SHA-256 of the UTF-8 bytes of [`normalize`]'s output, as 64 lowercase
/// hex characters. This is the cross-file content identity.
pub fn norm_text_sha256(input: &str) -> String {
    let digest = Sha256::digest(normalize(input).as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing formatted output into a String is infallible.
        write!(hex, "{byte:02x}").expect("String write cannot fail");
    }
    hex
}

/// Step 1: collapse every hard line break to a single LF, so later
/// whitespace logic only has to reason about `\n`.
fn normalize_line_endings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // Consume the LF of a CRLF pair so it counts as one break.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            // Vertical tab, form feed, and the Unicode next-line,
            // line-separator and paragraph-separator characters are all
            // hard line breaks; collapse them to LF like CRLF and CR.
            '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}' => out.push('\n'),
            _ => out.push(c),
        }
    }
    out
}

/// Step 3: drop zero-width and invisible characters that carry no
/// textual content and only differ between extractions.
fn strip_invisibles(s: &str) -> String {
    s.chars().filter(|c| !is_invisible(*c)).collect()
}

/// Zero-width / invisible characters removed by step 3: zero-width
/// space, zero-width non-joiner, zero-width joiner, word joiner,
/// BOM / zero-width no-break space, and soft hyphen.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}'
    )
}

/// Step 4: collapse punctuation encodings that differ only in code
/// point, not in meaning. Corner brackets and book-title marks are
/// left untouched — they are a distinct quoting system, not a styling
/// of these marks.
fn unify_punctuation(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{2015}' => out.push('\u{2014}'), // horizontal bar -> em dash
            '\u{2E3A}' => out.push_str("\u{2014}\u{2014}"), // two-em dash
            '\u{2E3B}' => out.push_str("\u{2014}\u{2014}\u{2014}"), // three-em dash
            '\u{2011}' => out.push('\u{2010}'), // non-breaking hyphen -> hyphen
            '\u{22EF}' => out.push('\u{2026}'), // midline ellipsis -> ellipsis
            '\u{201C}' | '\u{201D}' => out.push('"'), // curly double quotes
            '\u{2018}' | '\u{2019}' => out.push('\''), // curly single quotes
            _ => out.push(c),
        }
    }
    out
}

/// Step 5: convert tabs to a plain space. NFKC has already folded the
/// exotic Unicode spaces (no-break, ideographic, en/em, ...) to U+0020;
/// the tab is the one horizontal whitespace character it leaves alone.
fn horizontal_whitespace_to_space(s: &str) -> String {
    s.replace('\t', " ")
}

/// Step 6: trim each line's leading and trailing spaces, then collapse
/// internal runs of spaces to one. Newlines separate the lines and are
/// preserved.
fn fold_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut prev_space = false;
        for c in line.trim_matches(' ').chars() {
            if c == ' ' {
                if !prev_space {
                    out.push(' ');
                }
                prev_space = true;
            } else {
                out.push(c);
                prev_space = false;
            }
        }
    }
    out
}

/// Step 7: delete a single space that has a CJK character on both
/// sides. Extractors sometimes insert stray spaces between CJK
/// characters, and folding alone cannot remove them. A space with
/// non-CJK text on either side is kept — there it may be a real word
/// boundary. Each space is judged against its neighbours in the input
/// to this step, independently of deletions elsewhere.
fn delete_cjk_flanked_space(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        let cjk_flanked = c == ' '
            && i > 0
            && i + 1 < chars.len()
            && is_cjk(chars[i - 1])
            && is_cjk(chars[i + 1]);
        if !cjk_flanked {
            out.push(c);
        }
    }
    out
}

/// The "CJK character" set for the step 7 adjacency test: ideographs
/// (BMP, extension A, compatibility, and the supplementary planes),
/// hiragana and katakana, and the CJK symbols and punctuation block.
fn is_cjk(c: char) -> bool {
    matches!(
        u32::from(c),
        0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x3FFFF
    )
}

/// Step 8: collapse a run of three or more newlines (two or more blank
/// lines) to exactly two, so at most one blank line survives.
fn fold_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut newline_run = 0usize;
    for c in s.chars() {
        if c == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                out.push('\n');
            }
        } else {
            newline_run = 0;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // CJK ideographs are written as `\u{...}` escapes so the source
    // file stays ASCII-only; the comments give their meaning.

    #[test]
    fn line_endings_collapse_to_lf() {
        assert_eq!(normalize("a\r\nb"), "a\nb");
        assert_eq!(normalize("a\rb"), "a\nb");
        assert_eq!(normalize("a\u{000B}b"), "a\nb"); // vertical tab
        assert_eq!(normalize("a\u{000C}b"), "a\nb"); // form feed
        assert_eq!(normalize("a\u{0085}b"), "a\nb"); // next line (NEL)
        assert_eq!(normalize("a\u{2028}b"), "a\nb"); // line separator
        assert_eq!(normalize("a\u{2029}b"), "a\nb"); // paragraph separator
    }

    #[test]
    fn nfkc_folds_fullwidth_forms() {
        assert_eq!(normalize("\u{FF11}"), "1"); // fullwidth digit one
        assert_eq!(normalize("a\u{FF0C}b"), "a,b"); // fullwidth comma
    }

    #[test]
    fn invisibles_are_stripped() {
        assert_eq!(normalize("a\u{200B}b"), "ab"); // zero-width space
        assert_eq!(normalize("\u{FEFF}ab"), "ab"); // byte order mark
        assert_eq!(normalize("a\u{00AD}b"), "ab"); // soft hyphen
        assert_eq!(normalize("a\u{200D}b"), "ab"); // zero-width joiner
    }

    #[test]
    fn punctuation_is_unified() {
        assert_eq!(normalize("a\u{2015}b"), "a\u{2014}b"); // horizontal bar
        assert_eq!(normalize("a\u{2E3A}b"), "a\u{2014}\u{2014}b"); // two-em dash
        assert_eq!(normalize("\u{201C}x\u{201D}"), "\"x\""); // curly double
        assert_eq!(normalize("\u{2018}x\u{2019}"), "'x'"); // curly single
        assert_eq!(normalize("a\u{22EF}b"), "a\u{2026}b"); // midline ellipsis
    }

    #[test]
    fn whitespace_is_folded() {
        assert_eq!(normalize("a\tb"), "a b"); // tab -> space
        assert_eq!(normalize("a   b"), "a b"); // run of spaces
        assert_eq!(normalize("  a  "), "a"); // line trim
    }

    #[test]
    fn cjk_flanked_space_is_deleted() {
        // "ni" + space + "hao": stray space between two ideographs.
        assert_eq!(normalize("\u{4F60} \u{597D}"), "\u{4F60}\u{597D}");
        // A space with non-CJK on one side is a possible word boundary.
        assert_eq!(normalize("ab \u{4F60}"), "ab \u{4F60}");
        assert_eq!(normalize("\u{4F60} ab"), "\u{4F60} ab");
        assert_eq!(normalize("a b"), "a b");
    }

    #[test]
    fn blank_line_runs_collapse_to_one() {
        assert_eq!(normalize("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(normalize("a\n\nb"), "a\n\nb"); // one blank line kept
        assert_eq!(normalize("a\nb"), "a\nb"); // single newline kept
    }

    #[test]
    fn empty_and_whitespace_only_yield_empty() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   \n  \n "), "");
    }

    #[test]
    fn noise_injection_collapses_to_the_same_hash() {
        // "ni hao shi jie" — four CJK ideographs, canonical clean form.
        let clean = "\u{4F60}\u{597D}\u{4E16}\u{754C}";
        // The same content with every removed-noise class injected:
        // BOM, CJK-flanked space, zero-width space, ideographic space,
        // CRLF, and trailing spaces.
        let noisy = "\u{FEFF}\u{4F60} \u{597D}\u{200B}\u{4E16}\u{3000}\u{754C}\r\n  ";
        assert_eq!(normalize(noisy), clean);
        assert_eq!(norm_text_sha256(noisy), norm_text_sha256(clean));
    }

    #[test]
    fn normalize_is_idempotent() {
        let once = normalize("\u{FEFF}a  b\r\n\r\n\r\nc ");
        assert_eq!(normalize(&once), once);
    }

    #[test]
    fn hash_is_64_lowercase_hex() {
        let h = norm_text_sha256("anything");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    }

    #[test]
    fn hash_of_empty_input_is_the_empty_sha256() {
        assert_eq!(
            norm_text_sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn normalize_version_is_one() {
        assert_eq!(NORMALIZE_VERSION, 1);
    }
}
