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

use crate::contract::{Block, BlockKind, BlockStyle, ExtractError, SourceOfStructure, Toc};
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

/// Color one paper's block stream with heading and caption classifications.
///
/// The pass is purely additive: it walks `blocks` in document order,
/// promotes [`BlockKind::Body`] blocks to [`BlockKind::Heading`] or
/// [`BlockKind::Caption`] based on (1) the PDF outline that
/// [`crate::pdf::build_toc`] already attached to `toc`, and (2) a
/// text-pattern + geometry heuristic over [`Block::style`]. Blocks
/// outside [`BlockKind::Body`] are left untouched, so a second pass
/// over the same input is a no-op.
///
/// The returned [`SourceOfStructure`] records which signal contributed
/// — `Outline` when only the outline path produced hits, `Heuristic`
/// when only the rule set did, `Mixed` when both did, and `None` when
/// no heading was identified at all (the paper appears flat).
pub fn extract_paper_structured(blocks: &mut [Block], toc: &Toc) -> SourceOfStructure {
    let mut outline_hits = 0usize;
    let mut heuristic_hits = 0usize;

    // 1. Outline path. A `/Outline` that resolves at least two block
    // anchors and reports depth ≥ 1 (i.e. holds a non-flat tree once a
    // root entry is excluded) is treated as authoritative for those
    // anchored blocks; everything else falls through to the heuristic.
    let outline_usable = toc
        .entries
        .iter()
        .filter(|e| e.start_block.is_some())
        .count()
        >= 2
        && toc.entries.iter().map(|e| e.depth).max().unwrap_or(0) >= 1;
    if outline_usable {
        for entry in &toc.entries {
            let Some(idx) = entry.start_block else {
                continue;
            };
            let Some(block) = blocks.get_mut(idx) else {
                continue;
            };
            if matches!(block.kind, BlockKind::Body) {
                let level = if entry.depth == 0 { 1 } else { 2 };
                block.kind = BlockKind::Heading { level };
                outline_hits += 1;
            }
        }
    }

    // 2. Caption coloring. A short body block whose head matches the
    // figure / table introducer regex switches to Caption. Runs before
    // the heading heuristic so a captioned line never enters the
    // candidate pool.
    for block in blocks.iter_mut() {
        if !matches!(block.kind, BlockKind::Body) {
            continue;
        }
        if CAPTION_HEAD.is_match(block.text.trim_start()) {
            block.kind = BlockKind::Caption;
        }
    }

    // 3. Heuristic heading scoring. Per-page font-size median anchors
    // the "larger than body" signal; blocks with no `style` (older
    // envelopes, OCR, non-PDF adapters) skip the pass entirely and stay
    // Body — the outline path is the only way they can be colored.
    let page_median = page_font_medians(blocks);
    let mut candidates: Vec<(usize, u8)> = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        if !matches!(block.kind, BlockKind::Body) {
            continue;
        }
        let Some(style) = block.style.as_ref() else {
            continue;
        };
        let page_med = page_median
            .iter()
            .find(|(page, _)| *page == block.source_unit)
            .map(|(_, m)| *m)
            .unwrap_or(0.0);
        if let Some(level) = score_as_heading(&block.text, style, page_med) {
            candidates.push((i, level));
        }
    }

    // 4. Cross-page running-header dedup. Candidates whose
    // case-folded, whitespace-collapsed text recurs on three or more
    // distinct pages are dropped wholesale — these are the running
    // headers / journal mastheads `build_blocks` cannot filter (they
    // sit above the 80-char short-paragraph cap or carry per-page
    // tail noise that breaks exact-text dedup).
    let mut buckets: Vec<(String, Vec<(usize, u32)>)> = Vec::new();
    for &(i, _) in &candidates {
        let key = dedup_key(&blocks[i].text);
        if let Some(bucket) = buckets.iter_mut().find(|(k, _)| k == &key) {
            bucket.1.push((i, blocks[i].source_unit));
        } else {
            buckets.push((key, vec![(i, blocks[i].source_unit)]));
        }
    }
    let mut dropped: Vec<usize> = Vec::new();
    for (_, entries) in &buckets {
        let mut pages: Vec<u32> = entries.iter().map(|(_, p)| *p).collect();
        pages.sort_unstable();
        pages.dedup();
        if pages.len() >= 3 {
            dropped.extend(entries.iter().map(|(i, _)| *i));
        }
    }

    for (i, level) in candidates {
        if dropped.contains(&i) {
            continue;
        }
        if matches!(blocks[i].kind, BlockKind::Body) {
            blocks[i].kind = BlockKind::Heading { level };
            heuristic_hits += 1;
        }
    }

    match (outline_hits, heuristic_hits) {
        (0, 0) => SourceOfStructure::None,
        (_, 0) => SourceOfStructure::Outline,
        (0, _) => SourceOfStructure::Heuristic,
        (_, _) => SourceOfStructure::Mixed,
    }
}

/// Per-page median of `style.font_size_median` over the blocks the
/// score function reads as body candidates. Page entries are returned
/// in first-encounter order; the small linear lookup at the call site
/// is faster than building a hash map for the handful of pages a paper
/// holds.
fn page_font_medians(blocks: &[Block]) -> Vec<(u32, f32)> {
    let mut by_page: Vec<(u32, Vec<f32>)> = Vec::new();
    for block in blocks {
        let Some(style) = block.style.as_ref() else {
            continue;
        };
        if let Some((_, v)) = by_page.iter_mut().find(|(p, _)| *p == block.source_unit) {
            v.push(style.font_size_median);
        } else {
            by_page.push((block.source_unit, vec![style.font_size_median]));
        }
    }
    by_page
        .into_iter()
        .map(|(page, mut sizes)| {
            sizes.sort_by(f32::total_cmp);
            let median = if sizes.is_empty() {
                0.0
            } else {
                sizes[sizes.len() / 2]
            };
            (page, median)
        })
        .collect()
}

/// Score one body block against the pdffigures2 SectionTitleExtractor
/// rule set and return the heading level if it survives.
///
/// Rejections come first (a block that fails any one is not a heading
/// no matter how the rest score); additive signals follow and produce
/// a heading when at least two coincide. The level comes from the
/// numbered-prefix matcher when present, otherwise defaults to 1.
fn score_as_heading(text: &str, style: &BlockStyle, page_font_median: f32) -> Option<u8> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if style.line_count > 3 {
        return None;
    }
    let first = trimmed.chars().next()?;
    if first.is_ascii_lowercase() {
        return None;
    }
    let lower = ascii_lower(trimmed);
    if HEADING_BLACKLIST_PREFIXES
        .iter()
        .any(|p| lower.starts_with(p))
    {
        return None;
    }
    if TEMPLATE_HEAD.is_match(trimmed) {
        return None;
    }
    if math_unicode_ratio(trimmed) > 0.4 {
        return None;
    }

    let mut score: u8 = 0;
    let mut level: u8 = 1;

    if page_font_median > 0.1 && style.font_size_median > page_font_median + 0.5 {
        score += 1;
    }
    if style.is_bold_majority {
        score += 1;
    }
    if style.above_gap_ratio > 1.2 {
        score += 1;
    }
    if let Some(numbered_level) = numbered_level(trimmed) {
        score += 1;
        level = numbered_level;
    }

    if score >= 2 { Some(level) } else { None }
}

/// Map a numbered prefix to a heading depth. Supported families:
///
/// - Arabic `1.` / `1.1` / `1.1.1` → depth = number of dot-separated
///   components, capped at 3.
/// - Arabic `1` followed by a space and non-digit text → depth 1.
/// - Upper-case Roman numerals `IV.` / `XII` → depth 1.
/// - ASCII `A.` / `B.` (single letter + period) → depth 2.
/// - `Appendix` (case-insensitive) → depth 1.
/// - CJK ordinal headings `\u{7b2c}{count}\u{7ae0}` / `\u{7b2c}{count}\u{8282}`
///   ("Chapter / Section N") → depth 1 / 2.
fn numbered_level(text: &str) -> Option<u8> {
    if APPENDIX_RE.is_match(text) {
        return Some(1);
    }
    if let Some(caps) = ARABIC_NUMBERED_RE.captures(text) {
        let prefix = caps.get(1)?.as_str();
        let depth = prefix.split('.').count() as u8;
        return Some(depth.min(3));
    }
    if ARABIC_BARE_RE.is_match(text) {
        return Some(1);
    }
    if ROMAN_RE.is_match(text) {
        return Some(1);
    }
    if LETTER_RE.is_match(text) {
        return Some(2);
    }
    // \u{7b2c}\u{4e00}\u{7ae0} / \u{7b2c}1\u{7ae0} / \u{7b2c} 1 \u{7ae0}
    if CJK_CHAPTER_RE.is_match(text) {
        return Some(1);
    }
    if CJK_SECTION_RE.is_match(text) {
        return Some(2);
    }
    None
}

/// A non-allocating ASCII-only lower-case for the blacklist check. The
/// non-ASCII bytes are passed through verbatim; the blacklist itself is
/// ASCII so the comparison stays correct on mixed-script headings.
fn ascii_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Share of characters in `text` that fall in the Unicode mathematical
/// operator / symbol / supplemental-operator blocks. Above 40 % the
/// block is treated as an equation, not a heading.
fn math_unicode_ratio(text: &str) -> f32 {
    let mut math = 0usize;
    let mut total = 0usize;
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        total += 1;
        let v = c as u32;
        // U+2200..U+22FF Mathematical Operators;
        // U+27C0..U+27EF Miscellaneous Mathematical Symbols-A;
        // U+2980..U+29FF Miscellaneous Mathematical Symbols-B;
        // U+2A00..U+2AFF Supplemental Mathematical Operators.
        if (0x2200..=0x22FF).contains(&v)
            || (0x27C0..=0x27EF).contains(&v)
            || (0x2980..=0x29FF).contains(&v)
            || (0x2A00..=0x2AFF).contains(&v)
        {
            math += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        math as f32 / total as f32
    }
}

/// Case-folded, whitespace-collapsed key for cross-page heading dedup.
/// Two running-header candidates that differ only in surrounding white
/// space or letter case land in the same bucket.
fn dedup_key(text: &str) -> String {
    let mut buf = String::with_capacity(text.len());
    let mut last_space = true;
    for c in text.chars() {
        if c.is_whitespace() {
            if !last_space {
                buf.push(' ');
                last_space = true;
            }
            continue;
        }
        if c.is_ascii_uppercase() {
            buf.push(c.to_ascii_lowercase());
        } else {
            buf.push(c);
        }
        last_space = false;
    }
    buf.trim().to_string()
}

/// Heading prefixes the scoring stage rejects outright. Each entry is
/// matched against the ASCII-lower-cased candidate; non-ASCII tail
/// passes through untouched. Drawn from pdffigures2 plus the
/// boilerplate phrases the Phase 3 spike flagged as common false
/// positives over the validation set.
const HEADING_BLACKLIST_PREFIXES: &[&str] = &[
    "we ",
    "the ",
    "in ",
    "this ",
    "proceedings of",
    "preprint",
    "copyright",
    "doi:",
    "https://",
    "http://",
];

static CAPTION_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    // English "Figure 1.", "Fig 1:", "Table 2-3.", plus the Chinese
    // counterparts "\u{56fe} 1" and "\u{8868} 2". Anchored at the start
    // of the trimmed block text.
    Regex::new(concat!(
        r"^(?:Figure|Fig\.?|Table|",
        "\u{56fe}|\u{8868}",
        r")\s*\d+(?:[.:\-]|\s+\S)",
    ))
    .expect("caption head regex")
});

static TEMPLATE_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?i)(Definition|Theorem|Lemma|Corollary|Proposition|Remark|Example|Claim|Fact|Algorithm)\s+\d")
        .expect("template head regex")
});

static APPENDIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?i)Appendix(\s|\.|:)").expect("appendix regex"));

static ARABIC_NUMBERED_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Captures the dotted prefix so the caller can count depth.
    Regex::new(r"^([1-9][0-9]*(?:\.[0-9]+){0,2})\.?\s+\S").expect("arabic numbered regex")
});

static ARABIC_BARE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[1-9][0-9]*\s+[A-Za-z\u{4e00}-\u{9fff}]").expect("arabic bare regex")
});

static ROMAN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[IVX]+\.\s+\S").expect("roman regex"));

static LETTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z]\.\s+\S").expect("letter regex"));

static CJK_CHAPTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        "^\u{7b2c}",
        r"\s*[\u{4e00}-\u{9fff}0-9]+\s*",
        "\u{7ae0}",
    ))
    .expect("cjk chapter regex")
});

static CJK_SECTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        "^\u{7b2c}",
        r"\s*[\u{4e00}-\u{9fff}0-9]+\s*",
        "\u{8282}",
    ))
    .expect("cjk section regex")
});

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

    fn body_block(text: &str, page: u32, style: Option<BlockStyle>) -> Block {
        Block {
            kind: BlockKind::Body,
            text: text.to_string(),
            source_unit: page,
            style,
        }
    }

    fn style(font_size: f32, bold: bool, above_gap_ratio: f32) -> BlockStyle {
        BlockStyle {
            font_size_median: font_size,
            font_size_p90: font_size,
            is_bold_majority: bold,
            line_count: 1,
            x0_first_line: 72.0,
            above_gap_ratio,
        }
    }

    #[test]
    fn outline_path_promotes_anchored_blocks() {
        let mut blocks = vec![
            body_block("Title page text.", 0, Some(style(10.0, false, 0.0))),
            body_block("1. Introduction", 1, Some(style(10.0, false, 0.0))),
            body_block("Body of intro.", 1, Some(style(10.0, false, 0.0))),
            body_block("1.1 Motivation", 1, Some(style(10.0, false, 0.0))),
            body_block("Body of motivation.", 1, Some(style(10.0, false, 0.0))),
        ];
        let toc = Toc {
            entries: vec![
                crate::contract::TocEntry {
                    label: "Introduction".into(),
                    depth: 0,
                    start_block: Some(1),
                },
                crate::contract::TocEntry {
                    label: "Motivation".into(),
                    depth: 1,
                    start_block: Some(3),
                },
            ],
        };
        let sos = extract_paper_structured(&mut blocks, &toc);
        assert_eq!(sos, SourceOfStructure::Outline);
        assert_eq!(blocks[1].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[3].kind, BlockKind::Heading { level: 2 });
        assert_eq!(blocks[0].kind, BlockKind::Body);
        assert_eq!(blocks[2].kind, BlockKind::Body);
    }

    #[test]
    fn heuristic_path_picks_numbered_bold_lines() {
        // Body baseline at 10 pt; headings at 13 pt, bold, with extra
        // space above. No outline.
        let body_style = || Some(style(10.0, false, 0.5));
        let heading_style = || Some(style(13.0, true, 1.5));
        let mut blocks = vec![
            body_block("Some opening text.", 1, body_style()),
            body_block("1. Introduction", 2, heading_style()),
            body_block("The introduction body runs here.", 2, body_style()),
            body_block("2. Related Work", 3, heading_style()),
            body_block("Related work body.", 3, body_style()),
        ];
        let sos = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(sos, SourceOfStructure::Heuristic);
        assert_eq!(blocks[1].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[3].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[0].kind, BlockKind::Body);
        assert_eq!(blocks[2].kind, BlockKind::Body);
        assert_eq!(blocks[4].kind, BlockKind::Body);
    }

    #[test]
    fn heuristic_dedups_running_header_across_three_pages() {
        let heading_style = || Some(style(13.0, true, 1.5));
        // The same running-header text appears on three pages: it
        // would otherwise score as a heading on each page, but the
        // cross-page dedup drops every instance.
        let mut blocks = vec![
            body_block("Body line one.", 0, Some(style(10.0, false, 0.5))),
            body_block("Sample Journal Vol. 7", 0, heading_style()),
            body_block("Body line two.", 1, Some(style(10.0, false, 0.5))),
            body_block("Sample Journal Vol. 7", 1, heading_style()),
            body_block("Body line three.", 2, Some(style(10.0, false, 0.5))),
            body_block("Sample Journal Vol. 7", 2, heading_style()),
        ];
        let sos = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(sos, SourceOfStructure::None);
        for block in &blocks {
            assert_eq!(block.kind, BlockKind::Body, "{:?}", block.text);
        }
    }

    #[test]
    fn caption_coloring_matches_figure_and_chinese_table() {
        let body_style = || Some(style(10.0, false, 0.5));
        let mut blocks = vec![
            body_block("Figure 1. An illustration of the method.", 1, body_style()),
            body_block("Fig. 2: A second illustration.", 2, body_style()),
            body_block("Table 3. Numbers and more numbers.", 3, body_style()),
            // "\u{8868} 4" → "Table 4" in Chinese.
            body_block(
                "\u{8868} 4 \u{8bf4}\u{660e}\u{5b9e}\u{9a8c}\u{53c2}\u{6570}",
                4,
                body_style(),
            ),
            body_block("Just running prose, no caption.", 5, body_style()),
        ];
        let _ = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(blocks[0].kind, BlockKind::Caption);
        assert_eq!(blocks[1].kind, BlockKind::Caption);
        assert_eq!(blocks[2].kind, BlockKind::Caption);
        assert_eq!(blocks[3].kind, BlockKind::Caption);
        assert_eq!(blocks[4].kind, BlockKind::Body);
    }

    #[test]
    fn empty_signals_return_source_of_structure_none() {
        // No outline, no styles → heuristic skips every block.
        let mut blocks = vec![
            body_block("Just prose.", 0, None),
            body_block("More prose.", 1, None),
        ];
        let sos = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(sos, SourceOfStructure::None);
        assert!(blocks.iter().all(|b| matches!(b.kind, BlockKind::Body)));
    }

    #[test]
    fn numbered_level_decodes_dotted_prefix_depth() {
        assert_eq!(numbered_level("1. Introduction"), Some(1));
        assert_eq!(numbered_level("1.2 Background"), Some(2));
        assert_eq!(numbered_level("3.4.5 Inner section"), Some(3));
        assert_eq!(numbered_level("Appendix A: Details"), Some(1));
        assert_eq!(numbered_level("IV. Discussion"), Some(1));
        assert_eq!(numbered_level("A. First letter section"), Some(2));
        // CJK: "\u{7b2c}1\u{7ae0}" = "Chapter 1"; "\u{7b2c}1\u{8282}" = "Section 1".
        assert_eq!(
            numbered_level("\u{7b2c}1\u{7ae0} \u{5f15}\u{8a00}"),
            Some(1)
        );
        assert_eq!(
            numbered_level("\u{7b2c}1\u{8282} \u{80cc}\u{666f}"),
            Some(2)
        );
        assert_eq!(numbered_level("Not numbered"), None);
    }

    #[test]
    fn template_blacklist_rejects_definition_and_theorem() {
        let s = style(13.0, true, 1.5);
        // "Definition 1." matches the template head — score_as_heading
        // refuses it even though geometry would otherwise vote yes.
        assert_eq!(score_as_heading("Definition 1. Some defn.", &s, 10.0), None);
        assert_eq!(score_as_heading("Theorem 3.2 Big result.", &s, 10.0), None);
        // "1. Introduction" with the same geometry passes.
        assert_eq!(score_as_heading("1. Introduction", &s, 10.0), Some(1));
    }

    #[test]
    fn math_unicode_ratio_rejects_equation_lines() {
        let s = style(13.0, true, 1.5);
        // Mostly mathematical operators (U+2200..U+22FF range).
        let math = "\u{2200}x\u{2208}\u{2115}: x\u{2264}x\u{2295}\u{2207}";
        assert_eq!(score_as_heading(math, &s, 10.0), None);
    }
}
