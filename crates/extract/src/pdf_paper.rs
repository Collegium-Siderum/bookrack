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

use crate::contract::{Block, BlockKind, ExtractError, SourceOfStructure, Toc, TocEntry};
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

/// References-section heading that terminates the metadata-scan window.
///
/// Matched at line start, case-insensitive, against the
/// fullwidth-folded page text. The Chinese-form `\u{53C2}\u{8003}\u{6587}\u{732E}`
/// covers the two written orders (with and without an interior space)
/// seen in CJK journal layouts.
static REFS_HEADING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        "(?im)^\\s*(?:references|bibliography",
        "|\u{53C2}\\s*\u{8003}\\s*\u{6587}\\s*\u{732E}",
        ")\\b",
    ))
    .expect("static refs-heading regex compiles")
});

/// Maximum characters returned by [`extract_paper_metadata_text`]. The
/// cap caps the DOI / venue scan against very long front matter on
/// monograph-style PDFs and bounds worst-case allocation.
const MAX_METADATA_CHARS: usize = 30_000;

/// Take raw page text from a paper PDF, suitable for DOI / venue /
/// ISSN scans.
///
/// Returns the concatenated [`PdfPageText::all`] output from every
/// page up to (but not including) the first References-like heading,
/// with full-width Latin and digits folded to ASCII. The fold turns
/// PDFs that print their DOI as `\u{FF11}\u{FF10}\u{FF0E}\u{FF11}\u{FF18}\u{FF16}\u{FF15}\u{FF14}`
/// into something matchable by `\d`-based regex; CJK ideographs pass
/// through unchanged.
///
/// The window terminates at the References heading rather than at a
/// fixed page count so identifier scans cannot match a citation in the
/// bibliography. Chinese-language papers often hide the publisher's
/// DOI banner on page 2 or 3 after a cover sheet, so a fixed leading-
/// pages cap would miss those.
///
/// `Ok(None)` means the file decoded but produced no metadata-scan
/// text (an image-only paper). Errors are reported only for structural
/// failures opening the PDF or reading a page's text layer.
pub fn extract_paper_metadata_text(path: &Path) -> Result<Option<String>, ExtractError> {
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

    let mut out = String::new();
    for page in document.pages().iter() {
        let text = page.text().map_err(|e| ExtractError::CorruptFile {
            detail: e.to_string(),
        })?;
        out.push_str(&text.all());
        out.push('\n');
        if let Some(m) = REFS_HEADING_RE.find(&out) {
            out.truncate(m.start());
            break;
        }
        if out.chars().count() >= MAX_METADATA_CHARS {
            break;
        }
    }
    let folded = fold_fullwidth_to_ascii(&out);
    if folded.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(folded))
    }
}

/// Map full-width Latin letters, digits, and ASCII punctuation
/// (Unicode block U+FF01..U+FF5E) to their ASCII equivalents and the
/// ideographic space (U+3000) to a regular space. Leaves every other
/// code point alone, so CJK ideographs in the text remain intact.
fn fold_fullwidth_to_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\u{FF01}'..='\u{FF5E}' => {
                let mapped = char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch);
                out.push(mapped);
            }
            '\u{3000}' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// Color one paper's block stream with heading and caption classifications.
///
/// The pass is precision-first: it would rather miss a real heading
/// than admit a noise line that looks "heading-shaped." A caption
/// sub-step runs first, then two cooperating heading signals:
///
/// 1. **Caption pass.** Strict `Figure N.` / `Table N.` / Chinese
///    `\u{56fe}` / `\u{8868}` lines become [`BlockKind::Caption`]
///    before any heading work, so a captioned line never enters
///    either candidate pool below.
/// 2. **Outline pass.** Runs whenever the PDF's `/Outline` has any
///    entries. Each entry with a resolved `start_block` whose label
///    is not a figure / table caption (`FIGURE_CAPTION_LABEL`)
///    anchors a small forward window; the first block in that window
///    passing the strict heading gate and matching the entry's label
///    is promoted to `BlockKind::Heading { level }` (depth 0 → level
///    1, depth ≥ 1 → level 2). Anchors that find no matching block
///    in their window contribute nothing.
/// 3. **Strict-numbered heuristic.** Runs over every block the outline
///    pass left as `Body`. Candidates must pass the per-block gate
///    (numbered prefix, single line, short, no math, geometry
///    consistent with a heading) and then fit the ascending sequence
///    (1, 2, 3, … or I, II, III, …). Outline-promoted Headings are
///    absorbed as sequence anchors without being recounted, so an
///    outline-anchored "2 Background" lets the heuristic recognise
///    "3 Model Architecture" as the next L1 even when the outline
///    skips it.
///
/// The returned [`SourceOfStructure`] records which signal newly
/// promoted at least one block: `Outline`, `Heuristic`, `Mixed` when
/// both did, and `None` when neither produced a heading — that is an
/// explicit, safe-to-display answer, not a failure.
pub fn extract_paper_structured(blocks: &mut [Block], toc: &Toc) -> SourceOfStructure {
    // 1. Caption coloring. Strict head pattern: must have the
    //    introducer word + a number + a separator (period, colon,
    //    hyphen, or whitespace before non-space text). Runs first so
    //    a captioned line never enters the heading candidate pool.
    for block in blocks.iter_mut() {
        if !matches!(block.kind, BlockKind::Body) {
            continue;
        }
        if STRICT_CAPTION_HEAD.is_match(block.text.trim_start()) {
            block.kind = BlockKind::Caption;
        }
    }

    // 2. Outline-guided heading promotion. The PDF outline anchors a
    //    label to a page, and `crate::pdf::build_toc` resolves that to
    //    the first block on (or after) the target page. That anchor
    //    block is rarely the heading text itself — it is whatever
    //    block sat at the top of the page (a running header, an
    //    affiliation line, a continued paragraph). So instead of
    //    coloring the anchor directly, we walk a small window forward
    //    from it looking for a block that both passes the strict
    //    heading gate and matches the outline label. If we find one,
    //    that block is the heading.
    let mut outline_hits = 0usize;
    if !toc.entries.is_empty() {
        let entries: Vec<&TocEntry> = toc
            .entries
            .iter()
            .filter(|e| e.start_block.is_some())
            .filter(|e| !FIGURE_CAPTION_LABEL.is_match(e.label.trim_start()))
            .collect();
        for entry in entries {
            let Some(anchor) = entry.start_block else {
                continue;
            };
            if let Some(target) = locate_outline_heading(blocks, anchor, &entry.label) {
                let level = if entry.depth == 0 { 1 } else { 2 };
                blocks[target].kind = BlockKind::Heading { level };
                outline_hits += 1;
            }
        }
    }

    // 3. Strict-numbered heuristic over any block the outline pass
    //    left untouched. Sequence validation drops any candidate
    //    whose number does not advance the ascending series at its
    //    level. The validation walks every block in order — including
    //    the ones the outline pass already promoted — so an
    //    outline-anchored "2 Background" lets the heuristic recognise
    //    "3 Model Architecture" as the next L1 even though the outline
    //    skipped over it.
    let candidates = collect_strict_candidates(blocks);
    let accepted = validate_sequence(blocks, &candidates);
    let mut heuristic_hits = 0usize;
    for cand in accepted {
        if matches!(blocks[cand.block_idx].kind, BlockKind::Body) {
            blocks[cand.block_idx].kind = BlockKind::Heading { level: cand.level };
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

/// Walk forward from `anchor` searching for the body block whose
/// trimmed text matches the outline label after the leading numeric
/// prefix is stripped from both sides. Search ends once we leave the
/// anchor's page family — outline anchors that miss by more than one
/// page typically point at a removed section and produce noise when
/// matched too greedily.
fn locate_outline_heading(blocks: &[Block], anchor: usize, label: &str) -> Option<usize> {
    let label_key = normalize_label(label);
    if label_key.is_empty() {
        return None;
    }
    let anchor_page = blocks.get(anchor).map(|b| b.source_unit).unwrap_or(0);
    for idx in anchor..blocks.len() {
        let block = &blocks[idx];
        // Stay within the anchor's own page and the page after — the
        // outline is page-accurate within one page in practice.
        if block.source_unit > anchor_page + 1 {
            break;
        }
        if !matches!(block.kind, BlockKind::Body) {
            continue;
        }
        let trimmed = block.text.trim();
        let block_chars = trimmed.chars().count();
        // A real heading block stays close to its label in length —
        // 80 characters is the same cap the heuristic uses. Anything
        // longer is body text that swallowed the heading; matching it
        // produces "References [1] Hasan Abu-Rasheed…" style noise.
        if !(3..=80).contains(&block_chars) {
            continue;
        }
        // Block geometry must not be smaller than body text — a
        // sub-body font is a subscript / footnote marker, not a
        // heading. And a real heading is a single physical line: a
        // multi-line block has body text merged in, which produces
        // "Competing interests The authors declare …" noise on
        // tightly-set journal layouts.
        if let Some(style) = block.style.as_ref() {
            if style.line_count != 1 {
                continue;
            }
            let page_med = page_font_median(blocks, block.source_unit);
            if page_med > 0.1 && style.font_size_median < page_med * 0.95 {
                continue;
            }
        }
        let block_key = normalize_label(trimmed);
        if block_key.is_empty() {
            continue;
        }
        if block_key.starts_with(&label_key) || label_key.starts_with(&block_key) {
            return Some(idx);
        }
    }
    None
}

/// Lower-case the ASCII portion, strip any leading numeric / Roman
/// prefix, collapse runs of whitespace, and trim. The result is used
/// purely as a prefix-comparison key — never displayed to a user.
fn normalize_label(s: &str) -> String {
    let mut start = 0;
    let bytes = s.as_bytes();
    // Skip any leading "N.M.P " / "1." / "I." / "Appendix " sort of
    // numeric prefix so the comparison falls on the title words.
    while start < bytes.len() {
        let c = bytes[start];
        if c.is_ascii_digit() || matches!(c, b'.' | b' ' | b'\t' | b'I' | b'V' | b'X') {
            start += 1;
            continue;
        }
        break;
    }
    let tail = &s[start..];
    let mut out = String::with_capacity(tail.len());
    let mut last_space = true;
    for c in tail.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
            continue;
        }
        if c.is_ascii_uppercase() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
        last_space = false;
    }
    out.trim().to_string()
}

/// One block that survived every strict filter and is a candidate for
/// promotion to `BlockKind::Heading`. Sequence validation later
/// rejects any candidate whose numeric prefix does not advance the
/// ascending series at its level.
struct StrictCandidate {
    block_idx: usize,
    level: u8,
    numbers: Vec<u32>,
    is_roman: bool,
    is_appendix: bool,
}

/// Walk every Body block and yield those that pass the strict heading
/// gate: numbered prefix, single line, short text, no math operators,
/// no boilerplate prefix, geometry consistent with a heading. The
/// caller still has to validate the sequence — this pass is the
/// per-block filter, not the global decision.
fn collect_strict_candidates(blocks: &[Block]) -> Vec<StrictCandidate> {
    let mut out: Vec<StrictCandidate> = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        if !matches!(block.kind, BlockKind::Body) {
            continue;
        }
        let Some(style) = block.style.as_ref() else {
            continue;
        };
        if style.line_count != 1 {
            continue;
        }
        let trimmed = block.text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let char_count = trimmed.chars().count();
        if !(2..=80).contains(&char_count) {
            continue;
        }
        if contains_math_or_symbol(trimmed) {
            continue;
        }
        if trimmed.contains('@') {
            continue;
        }
        let lower = ascii_lower(trimmed);
        if BLACKLIST_PREFIXES.iter().any(|p| lower.starts_with(p)) {
            continue;
        }
        if TEMPLATE_HEAD.is_match(trimmed) {
            continue;
        }
        let Some((level, numbers, is_roman, is_appendix)) = parse_numbered_prefix(trimmed) else {
            continue;
        };
        // For Arabic-numbered candidates, require a corroborating
        // geometry signal — a bold majority OR a font noticeably
        // larger than the page median. A bare numbered line at body
        // weight is almost always a list item. Roman and Appendix
        // markers carry their own rarity: they almost never appear
        // mid-body and don't need a geometry signal to clear the gate.
        if !is_roman && !is_appendix {
            let page_med = page_font_median(blocks, block.source_unit);
            let geometry_ok = style.is_bold_majority
                || (page_med > 0.1 && style.font_size_median > page_med * 1.1);
            if !geometry_ok {
                continue;
            }
        }
        out.push(StrictCandidate {
            block_idx: i,
            level,
            numbers,
            is_roman,
            is_appendix,
        });
    }
    out
}

/// Walk every block in order and decide which candidates fit the
/// ascending section sequence. The walk interleaves two things:
///
/// 1. Any block that the outline pass already promoted to a
///    [`BlockKind::Heading`] is *absorbed* into the sequence state.
///    Its decoded number simply advances the state to that point —
///    accepted regardless of whether it would have matched
///    "expected next" from the previous state. This lets the outline
///    skip past a heading the heuristic would otherwise demand in
///    order.
/// 2. Strict candidates (still-Body blocks that passed
///    [`collect_strict_candidates`]) accept only when they match the
///    state's "expected next". The sequence model is precision-first:
///    a numbered jump (1 → 47) or a skipped level (1 then 1.1.1 with
///    no intermediate 1.1) drops the candidate.
///
/// `candidates` is the output of [`collect_strict_candidates`], in
/// block-index order.
fn validate_sequence<'a>(
    blocks: &[Block],
    candidates: &'a [StrictCandidate],
) -> Vec<&'a StrictCandidate> {
    let mut accepted: Vec<&StrictCandidate> = Vec::new();
    let mut l1: u32 = 0;
    let mut l2: u32 = 0;
    let mut l3: u32 = 0;
    let mut roman_l1: u32 = 0;
    let mut appendix_seen = false;
    let mut cand_iter = candidates.iter().peekable();

    for (idx, block) in blocks.iter().enumerate() {
        // First: absorb outline-promoted Headings into the state by
        // re-decoding their numbered prefix. The outline guarantees
        // the block is a real heading; we just need to know where it
        // sits in the sequence.
        if matches!(block.kind, BlockKind::Heading { .. }) {
            if let Some((level, numbers, is_roman, is_appendix)) =
                parse_numbered_prefix(block.text.trim())
            {
                if is_appendix {
                    appendix_seen = true;
                } else if is_roman {
                    if let Some(n) = numbers.first().copied() {
                        roman_l1 = n;
                    }
                } else {
                    match level {
                        1 => {
                            if let Some(n) = numbers.first().copied() {
                                l1 = n;
                                l2 = 0;
                                l3 = 0;
                            }
                        }
                        2 => {
                            if let (Some(a), Some(b)) =
                                (numbers.first().copied(), numbers.get(1).copied())
                            {
                                l1 = a;
                                l2 = b;
                                l3 = 0;
                            }
                        }
                        3 => {
                            if let (Some(a), Some(b), Some(c)) = (
                                numbers.first().copied(),
                                numbers.get(1).copied(),
                                numbers.get(2).copied(),
                            ) {
                                l1 = a;
                                l2 = b;
                                l3 = c;
                            }
                        }
                        _ => {}
                    }
                }
            }
            continue;
        }
        // Otherwise: if this block is a candidate at the current
        // index, check whether it advances the sequence.
        let Some(cand) = cand_iter.peek().filter(|c| c.block_idx == idx).copied() else {
            continue;
        };
        cand_iter.next();
        if cand.is_appendix {
            if !appendix_seen {
                appendix_seen = true;
                accepted.push(cand);
            }
            continue;
        }
        if cand.is_roman {
            let expected = roman_l1 + 1;
            if cand.numbers.first().copied() == Some(expected) {
                roman_l1 = expected;
                accepted.push(cand);
            }
            continue;
        }
        match cand.level {
            1 if cand.numbers.first().copied() == Some(l1 + 1) => {
                l1 += 1;
                l2 = 0;
                l3 = 0;
                accepted.push(cand);
            }
            2 if cand.numbers.first().copied() == Some(l1)
                && cand.numbers.get(1).copied() == Some(l2 + 1) =>
            {
                l2 += 1;
                l3 = 0;
                accepted.push(cand);
            }
            3 if cand.numbers.first().copied() == Some(l1)
                && cand.numbers.get(1).copied() == Some(l2)
                && cand.numbers.get(2).copied() == Some(l3 + 1) =>
            {
                l3 += 1;
                accepted.push(cand);
            }
            _ => {}
        }
    }
    accepted
}

/// Median of `style.font_size_median` over the Body blocks on the
/// requested page. Used as the "body baseline" against which the
/// heading heuristic asks whether a candidate is noticeably larger.
fn page_font_median(blocks: &[Block], page: u32) -> f32 {
    let mut sizes: Vec<f32> = blocks
        .iter()
        .filter(|b| b.source_unit == page && matches!(b.kind, BlockKind::Body))
        .filter_map(|b| b.style.as_ref().map(|s| s.font_size_median))
        .collect();
    if sizes.is_empty() {
        return 0.0;
    }
    sizes.sort_by(f32::total_cmp);
    sizes[sizes.len() / 2]
}

/// Parse the leading numbered prefix of a heading candidate. Returns
/// the heading level, the decoded numeric components, and the
/// Roman / Appendix flags that drive sequence validation.
///
/// Patterns, tried in order of specificity:
/// - `N.M.P` followed by a Latin or CJK letter — level 3.
/// - `N.M` followed by a Latin or CJK letter — level 2.
/// - `N.` or `N` followed by a Latin or CJK letter — level 1.
/// - `I.` / `IV.` Roman + Latin/CJK letter — level 1 Roman.
/// - `Appendix` — level 1 Appendix.
///
/// In every Arabic pattern the character following the number must be
/// a letter, not another digit. This single rule kills table-row
/// noise ("1 512 512 5.29") that previously sneaked through.
fn parse_numbered_prefix(trimmed: &str) -> Option<(u8, Vec<u32>, bool, bool)> {
    if APPENDIX_RE.is_match(trimmed) {
        return Some((1, Vec::new(), false, true));
    }
    if let Some(caps) = ARABIC_TRIPLE_RE.captures(trimmed) {
        let a: u32 = caps[1].parse().ok()?;
        let b: u32 = caps[2].parse().ok()?;
        let c: u32 = caps[3].parse().ok()?;
        return Some((3, vec![a, b, c], false, false));
    }
    if let Some(caps) = ARABIC_DOUBLE_RE.captures(trimmed) {
        let a: u32 = caps[1].parse().ok()?;
        let b: u32 = caps[2].parse().ok()?;
        return Some((2, vec![a, b], false, false));
    }
    if let Some(caps) = ARABIC_SINGLE_RE.captures(trimmed) {
        let a: u32 = caps[1].parse().ok()?;
        return Some((1, vec![a], false, false));
    }
    if let Some(caps) = ROMAN_RE.captures(trimmed) {
        let n = roman_value(&caps[1])?;
        return Some((1, vec![n], true, false));
    }
    None
}

/// Decode a 1..=30 Roman numeral in canonical subtractive form.
/// Returns `None` for syntactically valid but out-of-range inputs so a
/// stray `XL` token does not anchor a fake sequence, and also for
/// non-canonical spellings like `IIII` or `VV` that a naive left-to-right
/// add/subtract walker would otherwise accept.
fn roman_value(s: &str) -> Option<u32> {
    let mut total: i64 = 0;
    let mut prev: i64 = 0;
    for c in s.chars().rev() {
        let v: i64 = match c {
            'I' => 1,
            'V' => 5,
            'X' => 10,
            _ => return None,
        };
        if v < prev {
            total -= v;
        } else {
            total += v;
        }
        prev = v;
    }
    if !(1..=30).contains(&total) {
        return None;
    }
    let n = total as u32;
    // Round-trip against the single canonical encoding of `n`. Any
    // non-canonical input «`IIII`, `VV`, `VIIII`, `IIIIIII`...» now
    // sums to the right number but encodes differently from the
    // canonical form, so equality is the gate.
    (canonical_roman_within_thirty(n) == s).then_some(n)
}

/// Render `n` in the canonical Roman form for the 1..=30 sub-range
/// (no `L`/`C`/`D`/`M`, single subtractive pair per position).
fn canonical_roman_within_thirty(n: u32) -> String {
    let mut out = String::new();
    for _ in 0..(n / 10) {
        out.push('X');
    }
    match n % 10 {
        0 => {}
        ones @ 1..=3 => {
            for _ in 0..ones {
                out.push('I');
            }
        }
        4 => out.push_str("IV"),
        ones @ 5..=8 => {
            out.push('V');
            for _ in 0..(ones - 5) {
                out.push('I');
            }
        }
        9 => out.push_str("IX"),
        _ => unreachable!("n % 10 stays within 0..=9"),
    }
    out
}

/// Reject a block whose text contains any Unicode math operator,
/// arrow, technical symbol, or geometric shape. The strict heading
/// gate uses this to kill equation lines outright — recall on
/// equation-headed propositions like "1.2. Proposition. F = …" is
/// sacrificed deliberately.
fn contains_math_or_symbol(text: &str) -> bool {
    for c in text.chars() {
        let v = c as u32;
        if (0x2190..=0x21FF).contains(&v)    // Arrows
            || (0x2200..=0x22FF).contains(&v) // Mathematical Operators
            || (0x2300..=0x23FF).contains(&v) // Miscellaneous Technical
            || (0x25A0..=0x25FF).contains(&v) // Geometric Shapes (covers \u{25b3})
            || (0x27C0..=0x27EF).contains(&v) // Miscellaneous Mathematical Symbols-A
            || (0x2980..=0x29FF).contains(&v) // Miscellaneous Mathematical Symbols-B
            || (0x2A00..=0x2AFF).contains(&v)
        // Supplemental Mathematical Operators
        {
            return true;
        }
    }
    false
}

/// ASCII-only lower-casing for the boilerplate-prefix blacklist. CJK
/// and other non-ASCII characters pass through unchanged because they
/// are not part of any blacklist entry.
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

/// Heading prefixes the strict gate rejects outright. Each entry is
/// matched against the ASCII-lower-cased candidate; non-ASCII tail
/// passes through untouched.
const BLACKLIST_PREFIXES: &[&str] = &[
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
    "vol.",
    "volume ",
    "issn ",
    "issn:",
    "received ",
    "accepted ",
    "published ",
];

static STRICT_CAPTION_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:Figure|Fig\.?|Table|",
        "\u{56fe}|\u{8868}",
        r")\s*\d+(?:[.:\-]|\s+\S)",
    ))
    .expect("strict caption head regex")
});

static TEMPLATE_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?i)(Definition|Theorem|Lemma|Corollary|Proposition|Remark|Example|Claim|Fact|Algorithm)\s+\d")
        .expect("template head regex")
});

static APPENDIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?i)Appendix\b").expect("appendix regex"));

static ARABIC_TRIPLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([1-9][0-9]?)\.([1-9][0-9]?)\.([1-9][0-9]?)\.?\s+[A-Za-z\u{4e00}-\u{9fff}]")
        .expect("arabic triple regex")
});

static ARABIC_DOUBLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([1-9][0-9]?)\.([1-9][0-9]?)\.?\s+[A-Za-z\u{4e00}-\u{9fff}]")
        .expect("arabic double regex")
});

static ARABIC_SINGLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([1-9][0-9]?)\.?\s+[A-Za-z\u{4e00}-\u{9fff}]").expect("arabic single regex")
});

static ROMAN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([IVX]+)\.\s+[A-Za-z\u{4e00}-\u{9fff}]").expect("roman regex"));

static FIGURE_CAPTION_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?i)(?:Figure|Fig\.?|Table|",
        "\u{56fe}|\u{8868}",
        r"|Equation|Eq\.?|Algorithm)\s*\d",
    ))
    .expect("figure caption label regex")
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

    use crate::contract::{BlockStyle, TocEntry};

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
    fn outline_trusted_mode_colors_only_outline_entries() {
        // Outline anchors three real sections; the heuristic does not
        // run on top.
        let mut blocks = vec![
            body_block("Title page text.", 0, Some(style(14.0, true, 0.0))),
            body_block("1. Introduction", 1, Some(style(12.0, true, 1.5))),
            body_block("Body of intro.", 1, Some(style(10.0, false, 0.5))),
            body_block("1.1 Motivation", 1, Some(style(11.0, true, 1.2))),
            body_block("Body of motivation.", 1, Some(style(10.0, false, 0.5))),
            body_block("2. Related Work", 2, Some(style(12.0, true, 1.5))),
            body_block("Body of related.", 2, Some(style(10.0, false, 0.5))),
        ];
        let toc = Toc {
            entries: vec![
                TocEntry {
                    label: "Introduction".into(),
                    depth: 0,
                    start_block: Some(1),
                },
                TocEntry {
                    label: "Motivation".into(),
                    depth: 1,
                    start_block: Some(3),
                },
                TocEntry {
                    label: "Related Work".into(),
                    depth: 0,
                    start_block: Some(5),
                },
            ],
        };
        let sos = extract_paper_structured(&mut blocks, &toc);
        assert_eq!(sos, SourceOfStructure::Outline);
        assert_eq!(blocks[1].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[3].kind, BlockKind::Heading { level: 2 });
        assert_eq!(blocks[5].kind, BlockKind::Heading { level: 1 });
        // Page-0 title and every body block stay Body.
        assert_eq!(blocks[0].kind, BlockKind::Body);
        assert_eq!(blocks[2].kind, BlockKind::Body);
        assert_eq!(blocks[4].kind, BlockKind::Body);
        assert_eq!(blocks[6].kind, BlockKind::Body);
    }

    #[test]
    fn outline_with_only_figure_entries_falls_back_to_heuristic() {
        // The outline contains only figure / table anchors; after the
        // FIGURE_CAPTION_LABEL filter, the trust threshold of three
        // section-shaped anchors is not met.
        let mut blocks = vec![
            body_block("Caption 1.", 0, Some(style(10.0, false, 0.5))),
            body_block("Caption 2.", 1, Some(style(10.0, false, 0.5))),
            body_block("Caption 3.", 2, Some(style(10.0, false, 0.5))),
        ];
        let toc = Toc {
            entries: vec![
                TocEntry {
                    label: "Figure 1: a".into(),
                    depth: 0,
                    start_block: Some(0),
                },
                TocEntry {
                    label: "Figure 2: b".into(),
                    depth: 0,
                    start_block: Some(1),
                },
                TocEntry {
                    label: "Table 1: c".into(),
                    depth: 0,
                    start_block: Some(2),
                },
            ],
        };
        let sos = extract_paper_structured(&mut blocks, &toc);
        assert_eq!(
            sos,
            SourceOfStructure::None,
            "no usable outline → no heuristic input either"
        );
        assert!(blocks.iter().all(|b| matches!(b.kind, BlockKind::Body)));
    }

    #[test]
    fn strict_heuristic_accepts_ascending_arabic_sequence() {
        let heading = || Some(style(13.0, true, 1.5));
        let body = || Some(style(10.0, false, 0.5));
        let mut blocks = vec![
            // Page 0 noise — skipped wholesale.
            body_block("Some Big Title", 0, heading()),
            body_block("First Author Second Author", 0, heading()),
            // Real sections start page 1.
            body_block("1 Introduction", 1, heading()),
            body_block("Body of introduction.", 1, body()),
            body_block("2 Related Work", 2, heading()),
            body_block("3 Method", 3, heading()),
            body_block("3.1 Architecture", 3, heading()),
            body_block("3.2 Training", 3, heading()),
            body_block("4 Results", 4, heading()),
        ];
        let sos = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(sos, SourceOfStructure::Heuristic);
        // Page-0 blocks stay Body.
        assert_eq!(blocks[0].kind, BlockKind::Body);
        assert_eq!(blocks[1].kind, BlockKind::Body);
        // Real sections promoted.
        assert_eq!(blocks[2].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[4].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[5].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[6].kind, BlockKind::Heading { level: 2 });
        assert_eq!(blocks[7].kind, BlockKind::Heading { level: 2 });
        assert_eq!(blocks[8].kind, BlockKind::Heading { level: 1 });
    }

    #[test]
    fn sequence_check_rejects_table_row_starting_with_digit() {
        // "1 Introduction" passes, but "1 512 512 5.29" is a table row
        // — after the leading number comes another digit, so the
        // regex won't match it. Even if it did, the geometry of a
        // table row wouldn't pass the bold-or-larger filter.
        let heading = || Some(style(13.0, true, 1.5));
        let body = || Some(style(10.0, false, 0.5));
        let mut blocks = vec![
            body_block("1 Introduction", 1, heading()),
            body_block("Body line.", 1, body()),
            body_block("1 512 512 5.29 24.9", 1, body()),
            body_block("2 Related Work", 2, heading()),
        ];
        let _ = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(blocks[0].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[2].kind, BlockKind::Body, "table row stays Body");
        assert_eq!(blocks[3].kind, BlockKind::Heading { level: 1 });
    }

    #[test]
    fn sequence_check_rejects_out_of_order_numbered_block() {
        let heading = || Some(style(13.0, true, 1.5));
        let mut blocks = vec![
            body_block("1 Introduction", 1, heading()),
            body_block("47 Random Number", 2, heading()),
            body_block("2 Background", 3, heading()),
        ];
        let _ = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(blocks[0].kind, BlockKind::Heading { level: 1 });
        assert_eq!(
            blocks[1].kind,
            BlockKind::Body,
            "47 is not the next L1 after 1, drop it"
        );
        assert_eq!(blocks[2].kind, BlockKind::Heading { level: 1 });
    }

    #[test]
    fn sequence_check_rejects_orphan_subsection_under_wrong_parent() {
        // Without a matching L1, an L2 candidate has no parent.
        let heading = || Some(style(13.0, true, 1.5));
        let mut blocks = vec![
            body_block("1 Introduction", 1, heading()),
            body_block("3.1 Stray Subsection", 1, heading()),
        ];
        let _ = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(blocks[0].kind, BlockKind::Heading { level: 1 });
        assert_eq!(blocks[1].kind, BlockKind::Body);
    }

    #[test]
    fn math_lines_are_rejected_outright() {
        let heading = || Some(style(13.0, true, 1.5));
        let mut blocks = vec![
            body_block("1 Introduction", 1, heading()),
            // "2 Z (R + \u{25b3}f) 2 e \u{2212}f dV \u{2265}" contains
            // \u{25b3} (Geometric Shapes) and \u{2265} (Math Operators).
            body_block(
                "2 Z (R + \u{25b3}f) 2 e \u{2212}f dV \u{2265}",
                1,
                heading(),
            ),
            body_block("2 Background", 2, heading()),
        ];
        let _ = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(blocks[0].kind, BlockKind::Heading { level: 1 });
        assert_eq!(
            blocks[1].kind,
            BlockKind::Body,
            "math line rejected by Unicode block filter"
        );
        assert_eq!(blocks[2].kind, BlockKind::Heading { level: 1 });
    }

    #[test]
    fn unnumbered_lines_are_always_rejected() {
        // Even with strong geometry, an un-numbered line is not a
        // heading under the strict gate.
        let heading = || Some(style(13.0, true, 1.5));
        let mut blocks = vec![
            body_block("Introduction", 1, heading()),
            body_block("Related Work", 2, heading()),
        ];
        let sos = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(sos, SourceOfStructure::None);
        assert!(blocks.iter().all(|b| matches!(b.kind, BlockKind::Body)));
    }

    #[test]
    fn page_zero_is_accepted_when_it_carries_a_real_heading() {
        // Page 0 was special-cased through one iteration of the
        // smoke-test redesign and dropped wholesale, but losing
        // "1 Introduction" on conference-style two-column papers
        // (BERT, ACL, …) where sections start on page 0 hurt recall
        // for no precision gain. The strict gate (numbered prefix +
        // geometry + sequence) handles page 0 the same as any other
        // page.
        let heading = || Some(style(13.0, true, 1.5));
        let mut blocks = vec![body_block("1 Introduction", 0, heading())];
        let sos = extract_paper_structured(&mut blocks, &Toc::default());
        assert_eq!(sos, SourceOfStructure::Heuristic);
        assert_eq!(blocks[0].kind, BlockKind::Heading { level: 1 });
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
    fn parse_numbered_prefix_decodes_supported_families() {
        assert_eq!(
            parse_numbered_prefix("1. Introduction"),
            Some((1, vec![1], false, false))
        );
        assert_eq!(
            parse_numbered_prefix("1 Introduction"),
            Some((1, vec![1], false, false))
        );
        assert_eq!(
            parse_numbered_prefix("1.2 Background"),
            Some((2, vec![1, 2], false, false))
        );
        assert_eq!(
            parse_numbered_prefix("3.4.5 Inner"),
            Some((3, vec![3, 4, 5], false, false))
        );
        assert_eq!(
            parse_numbered_prefix("Appendix A: Details"),
            Some((1, vec![], false, true))
        );
        assert_eq!(
            parse_numbered_prefix("IV. Discussion"),
            Some((1, vec![4], true, false))
        );
        // After the number must be a letter, not a digit.
        assert_eq!(parse_numbered_prefix("1 512 5.29"), None);
        // Bare un-numbered text never matches.
        assert_eq!(parse_numbered_prefix("Introduction"), None);
        // A solo letter "A. Details" is no longer treated as a heading
        // marker — too noisy on table-of-contents style enumerations.
        assert_eq!(parse_numbered_prefix("A. First letter section"), None);
    }

    #[test]
    fn roman_numeral_value_clamps_to_safe_range() {
        assert_eq!(roman_value("I"), Some(1));
        assert_eq!(roman_value("IV"), Some(4));
        assert_eq!(roman_value("XII"), Some(12));
        assert_eq!(roman_value("XX"), Some(20));
        assert_eq!(roman_value("XXX"), Some(30));
        // Beyond 30 we reject — past that, real papers don't number
        // sections in Roman.
        assert_eq!(roman_value("XXXI"), None);
        // Non-Roman characters reject.
        assert_eq!(roman_value("XAB"), None);
    }

    #[test]
    fn roman_numeral_rejects_non_canonical_spellings() {
        // Repeated-letter forms are arithmetically correct but
        // violate the canonical subtractive encoding.
        assert_eq!(roman_value("IIII"), None);
        assert_eq!(roman_value("VIIII"), None);
        assert_eq!(roman_value("VV"), None);
        assert_eq!(roman_value("XXXXIII"), None);
        // The subtractive forms themselves still round-trip.
        assert_eq!(roman_value("IX"), Some(9));
        assert_eq!(roman_value("XIX"), Some(19));
        assert_eq!(roman_value("XXIV"), Some(24));
    }

    #[test]
    fn template_words_like_definition_or_theorem_are_blacklisted() {
        // "Definition 1. …" matches the template head and is rejected
        // by collect_strict_candidates, not by the sequence stage.
        let heading = || Some(style(13.0, true, 1.5));
        let mut blocks = vec![
            body_block("Definition 1. A defn.", 1, heading()),
            body_block("Theorem 3.2 Big result.", 1, heading()),
        ];
        let _ = extract_paper_structured(&mut blocks, &Toc::default());
        assert!(blocks.iter().all(|b| matches!(b.kind, BlockKind::Body)));
    }
}
