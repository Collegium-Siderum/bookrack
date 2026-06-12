// SPDX-License-Identifier: Apache-2.0

//! Paper-oriented PDF abstract extraction.
//!
//! Sits beside [`crate::pdf`]: same PDFium binding, same extraction
//! lock, different algorithm. The general PDF adapter rebuilds columns
//! and paragraphs at the character level — accurate for long-form
//! prose, but fragmenting and lossy on paper layouts where the abstract
//! is intermixed with running affiliation / funding / venue text. This
//! module takes PDFium's native reading-order text per page, scans for
//! an anchor word, then takes the body up to the next section marker.
//!
//! The algorithm has three steps:
//!
//! 1. Concatenate [`PdfPageText::all`] across every page, with newline
//!    separators so per-page text never bleeds together.
//! 2. Find the first anchor heading — `Abstract` / `ABSTRACT` /
//!    `Summary` / `SUMMARY` plus the Chinese-form abstract heading.
//!    Two passes: head-only (first 5000 bytes) first, then full
//!    document. The second pass exists for cover-paged journals where
//!    the first leaves are an issue masthead.
//! 3. From the anchor end, take text up to the first section marker —
//!    Keywords / Introduction / Categories and Subject / Index terms /
//!    CCS Concepts plus their Chinese-form counterparts — capped at
//!    3000 characters. Before scanning, a pre-normalization pass
//!    inserts a newline before any section marker that follows ASCII
//!    or full-width sentence-ending punctuation, so a marker pdfium
//!    emits mid-line is still seen as the start of its own logical
//!    line.
//!
//! Fallback: when no anchor matches anywhere, return the first two
//! pages of text (capped at 2000 characters). This is the right answer
//! for papers that genuinely lack an abstract heading (some Nature
//! Communications articles, arXiv math preprints, ACM CHI workshop
//! papers, perspective-style essays): the first pages already contain
//! title, authors, and the opening of the body, which is what a reader
//! sees first and what a downstream retriever indexes against.
//!
//! The combined regex / fallback policy was selected against a 16-paper
//! validation set spanning four PDF generators, six disciplines, single
//! / double / triple-column layouts, bilingual papers, cover-paged
//! journals, and PDFs with custom-encoded fonts (PUA / full-width
//! ASCII). The set produces 8 clean anchor hits, 4 anchor hits with
//! correct content but layout noise (footnotes / sidebar columns
//! mingled in), 4 reasonable fallbacks, and zero garbage outputs.
//!
//! ## Source encoding of CJK literals
//!
//! Every CJK code point this module matches is written as a Rust
//! `\u{XXXX}` escape rather than as raw UTF-8 bytes, per the repo-wide
//! rule that source files outside `*/tests/fixtures/` carry no CJK
//! bytes. The compiled string still contains the actual characters;
//! only the on-disk source is ASCII.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::contract::ExtractError;
use crate::pdf::{EXTRACTION_LOCK, pdfium};

/// Maximum bytes of the head scanned for an anchor in the first pass.
/// Five kilobytes covers a typical title page and first-page abstract;
/// failing the head pass triggers a full-document second scan.
const HEAD_BYTES: usize = 5_000;

/// Maximum characters returned as the abstract body. Captures even
/// long PLoS-style summary panels while bounding worst-case output.
const MAX_BODY_CHARS: usize = 3_000;

/// Maximum characters returned by the no-anchor fallback. Two pages of
/// dense Chinese text fit comfortably under this cap.
const MAX_FALLBACK_CHARS: usize = 2_000;

/// Number of leading pages the fallback returns when no anchor matches.
const FALLBACK_PAGES: usize = 2;

/// Minimum character count an anchored body must reach to be accepted.
/// A short result usually means the anchor matched a stray occurrence
/// of the word in mid-text rather than a real heading.
const MIN_ANCHOR_BODY_CHARS: usize = 80;

/// Section names that close the abstract. Used both for `STOP` (a
/// line-start match) and for `PRE_NORM` (a mid-line normalization
/// pass). CJK code points are spelled as `\u{...}` escapes so this
/// source file contains no raw CJK bytes; the compiled string holds
/// the actual characters.
const SECTION_ALT: &str = concat!(
    // Chinese keyword markers ("guan jian ci" / "guan jian zi"), then
    // their English counterparts.
    "\u{5173}\\s*\u{952E}\\s*\u{8BCD}",
    "|\u{5173}\\s*\u{952E}\\s*\u{5B57}",
    "|Key\\s*[Ww]ords?|Keywords|KEY\\s?WORDS",
    // Introduction-section markers, optionally prefixed by an
    // enumeration token. Last two alternates inside the inner group
    // are Chinese-form "yin yan" and "bei jing".
    "|(?:[1I\u{2160}\u{4E00}]\\s*[\\.\u{3001},]?\\s+)?",
    "(?:Introduction|INTRODUCTION|\u{5F15}\\s*\u{8A00}|\u{80CC}\\s*\u{666F})",
    // Common conference-paper indices.
    "|Categories\\s+and\\s+Subject|Index\\s+[Tt]erms|CCS\\s+CONCEPTS",
);

static ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    // Inline `\u{6458}\u{8981}` is the Chinese-form abstract heading;
    // `\u{FF1A}` is the full-width colon Chinese journals use after
    // headings.
    Regex::new(concat!(
        "(?m)^\\s*",
        "(?:(?P<cn>\u{6458}\\s*\u{8981})|",
        "(?P<en>Abstract|ABSTRACT|Summary|SUMMARY))",
        "\\s*[\u{FF1A}:.]?\\s*",
    ))
    .expect("static anchor regex compiles")
});

static STOP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("(?im)^\\s*(?:{SECTION_ALT})")).expect("static stop regex compiles")
});

static PRE_NORM: LazyLock<Regex> = LazyLock::new(|| {
    // Sentence-ending punctuation: ASCII period plus the CJK ideographic
    // full stop `\u{3002}`.
    Regex::new(&format!("([\u{3002}.])\\s+({SECTION_ALT})"))
        .expect("static pre-norm regex compiles")
});

/// The reason `extract_paper_abstract` produced its result.
///
/// `&'static str` shape matches the audit log convention the glean
/// pipeline uses for the existing block-level fallback.
pub mod reason {
    /// A Chinese-form anchor heading matched.
    pub const HEADING_CN: &str = "heading-cn";
    /// An English anchor (`Abstract` / `Summary` / their uppercase
    /// variants) matched.
    pub const HEADING_EN: &str = "heading-en";
    /// No anchor matched. The first two pages of text were returned.
    pub const FALLBACK_FIRST_PAGES: &str = "fallback-first-pages";
}

/// Locate the abstract in a paper-style PDF.
///
/// Returns `Ok(Some((text, reason)))` for both anchored hits and the
/// first-pages fallback. Returns `Ok(None)` only when the file is
/// empty enough that even the fallback produces no usable text.
/// Errors are reported only for structural failures opening the PDF
/// or reading a page's text layer.
pub fn extract_paper_abstract(path: &Path) -> Result<Option<(String, &'static str)>, ExtractError> {
    let pdfium = pdfium()?;
    let _guard = EXTRACTION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let document =
        pdfium
            .load_pdf_from_file(path, None)
            .map_err(|e| ExtractError::CorruptFile {
                detail: e.to_string(),
            })?;

    let mut full = String::new();
    let mut page_starts: Vec<usize> = Vec::new();
    for (i, page) in document.pages().iter().enumerate() {
        page_starts.push(full.len());
        if i > 0 {
            full.push('\n');
        }
        let text = page.text().map_err(|e| ExtractError::CorruptFile {
            detail: e.to_string(),
        })?;
        full.push_str(&text.all());
        full.push('\n');
    }

    // Two-pass anchor scan: head first, then the full document. The
    // head-only pass keeps body mentions of the anchor words in
    // related-work sections from masquerading as the heading.
    let head_end = char_boundary_at_or_below(&full, full.len().min(HEAD_BYTES));
    let captures = ANCHOR
        .captures(&full[..head_end])
        .or_else(|| ANCHOR.captures(&full));

    if let Some(captures) = captures {
        let kind = if captures.name("cn").is_some() {
            reason::HEADING_CN
        } else {
            reason::HEADING_EN
        };
        let m = captures.get(0).expect("regex always has a group 0");
        let body_raw = &full[m.end()..];
        let body_norm = PRE_NORM.replace_all(body_raw, "$1\n$2");
        let body = body_norm.as_ref();
        let stop_byte = STOP.find(body).map(|s| s.start()).unwrap_or(body.len());
        let abs_text = collapse_whitespace(&take_chars(&body[..stop_byte], MAX_BODY_CHARS));
        if abs_text.chars().count() >= MIN_ANCHOR_BODY_CHARS {
            return Ok(Some((abs_text, kind)));
        }
    }

    // Fallback: first two pages of text.
    let fallback_end = if page_starts.len() > FALLBACK_PAGES {
        page_starts[FALLBACK_PAGES]
    } else {
        full.len()
    };
    let chunk = take_chars(&full[..fallback_end], MAX_FALLBACK_CHARS);
    let out = collapse_whitespace(&chunk);
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some((out, reason::FALLBACK_FIRST_PAGES)))
    }
}

/// Round `idx` down to the nearest UTF-8 character boundary in `s`,
/// so slicing `s[..n]` does not split a multi-byte character.
fn char_boundary_at_or_below(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Take the first `max` characters of `s` as a new owned string.
fn take_chars(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        out.push(c);
    }
    out
}

/// Collapse runs of Unicode whitespace to single ASCII spaces and trim
/// the ends.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_whitespace_squeezes_runs() {
        assert_eq!(collapse_whitespace("a   b\n\nc\td"), "a b c d");
        assert_eq!(
            collapse_whitespace("   leading   trailing   "),
            "leading trailing"
        );
        assert_eq!(collapse_whitespace(""), "");
    }

    #[test]
    fn take_chars_counts_characters_not_bytes() {
        // "\u{4E2D}\u{6587}\u{6D4B}\u{8BD5}" is CJK "zhong wen ce shi".
        assert_eq!(
            take_chars("\u{4E2D}\u{6587}\u{6D4B}\u{8BD5}", 2),
            "\u{4E2D}\u{6587}"
        );
        assert_eq!(take_chars("abc", 10), "abc");
        assert_eq!(take_chars("", 5), "");
    }

    #[test]
    fn char_boundary_clamps_to_grapheme() {
        // "abc" + CJK "zhong wen".
        let s = "abc\u{4E2D}\u{6587}";
        assert_eq!(char_boundary_at_or_below(s, 4), 3);
        assert_eq!(char_boundary_at_or_below(s, 100), s.len());
    }

    #[test]
    fn anchor_regex_matches_english_inline_form() {
        let caps = ANCHOR.captures("\nAbstract: hello\n").unwrap();
        assert!(caps.name("en").is_some());
        assert!(caps.name("cn").is_none());
    }

    #[test]
    fn anchor_regex_matches_chinese_with_space() {
        // "\u{6458} \u{8981}" is the Chinese-form abstract heading
        // with a space between glyphs; "\u{FF1A}" is the full-width
        // colon Chinese journals print after a heading; the body
        // "\u{4F60}\u{597D}" is "ni hao".
        let caps = ANCHOR
            .captures("\n\u{6458} \u{8981}\u{FF1A}\u{4F60}\u{597D}\n")
            .unwrap();
        assert!(caps.name("cn").is_some());
        assert!(caps.name("en").is_none());
    }

    #[test]
    fn stop_regex_matches_keywords_at_line_start() {
        // "\u{5173}\u{952E}\u{8BCD}" is the Chinese-form keywords
        // section heading. The body "A\u{FF1B}B" uses a full-width
        // semicolon between the example terms.
        let body = "abstract body text.\n\u{5173}\u{952E}\u{8BCD}\u{FF1A}A\u{FF1B}B";
        let m = STOP.find(body).unwrap();
        assert!(body[m.start()..].starts_with("\u{5173}\u{952E}\u{8BCD}"));
    }

    #[test]
    fn pre_norm_breaks_line_before_inline_section_marker() {
        let body = "abstract body.   \u{5173}\u{952E}\u{8BCD} A\u{FF1B}B";
        let normalized = PRE_NORM.replace_all(body, "$1\n$2");
        assert!(normalized.contains(".\n\u{5173}\u{952E}\u{8BCD}"));
    }
}
