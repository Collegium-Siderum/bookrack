// SPDX-License-Identifier: Apache-2.0

//! Local IDENTIFY pass for the glean pipeline. Pure pattern matching
//! over the extracted text — no network calls. Each detector returns
//! the canonical form (no `arXiv:` prefix, no `DOI:` prefix, no
//! trailing sentence punctuation) so callers can write the result
//! straight into the catalog.

use std::path::Path;
use std::sync::LazyLock;

use bookrack_extract::{Block, BlockKind, Extraction, extract_paper_abstract};
use regex::Regex;

use crate::AbstractStrategy;

/// DOI shape (CrossRef syntax): the registrant prefix
/// `10.NNNN[NNNNN]` joined by `/` to a suffix over the conservative
/// DOI character set.
static DOI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"10\.\d{4,9}/[-._;()/:A-Za-z0-9]+").expect("DOI regex"));

/// arXiv new-form identifier (April 2007 onwards): `NNNN.NNNNN`,
/// optionally followed by the version suffix `vN`. Capture group 1
/// isolates the bare identifier.
static ARXIV_NEW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{4}\.\d{4,5})(?:v\d+)?").expect("arXiv new regex"));

/// arXiv old-form identifier (pre-2007): `cat[.sub]/NNNNNNN`, with
/// optional `vN`. Capture group 1 isolates the bare identifier.
static ARXIV_OLD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([a-z][a-z\-]*(?:\.[A-Za-z\-]+)?/\d{7})(?:v\d+)?").expect("arXiv old regex")
});

/// ISSN: four digits, dash, three digits, then a check character
/// (digit or `X`).
static ISSN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{4}-\d{3}[\dX]\b").expect("ISSN regex"));

/// Venue cue phrases. A match seeds a line-level lookup that returns
/// the whole line as the venue string.
static VENUE_CUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\bproceedings of\b|\bconference on\b|\bvol\.)").expect("venue cue regex")
});

/// Return the first DOI found anywhere in `text`, with trailing
/// sentence punctuation stripped.
pub fn detect_doi(text: &str) -> Option<String> {
    DOI_RE
        .find(text)
        .map(|m| strip_trailing_punct(m.as_str()).to_string())
}

/// Return an arXiv identifier in canonical form (no `arXiv:` prefix,
/// no version suffix), preferring `info_title` over `footer_text`.
pub fn detect_arxiv_id(info_title: Option<&str>, footer_text: &str) -> Option<String> {
    for src in [info_title, Some(footer_text)].into_iter().flatten() {
        if let Some(id) = match_arxiv_in(src) {
            return Some(id);
        }
    }
    None
}

fn match_arxiv_in(text: &str) -> Option<String> {
    if let Some(caps) = ARXIV_NEW_RE.captures(text) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }
    if let Some(caps) = ARXIV_OLD_RE.captures(text) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }
    None
}

/// Return the line containing the first venue cue, with trailing
/// sentence punctuation stripped.
pub fn detect_venue(footer_text: &str) -> Option<String> {
    let cue = VENUE_CUE_RE.find(footer_text)?;
    let line_start = footer_text[..cue.start()].rfind('\n').map_or(0, |i| i + 1);
    let line_end = footer_text[cue.end()..]
        .find('\n')
        .map_or(footer_text.len(), |i| cue.end() + i);
    let line = footer_text[line_start..line_end].trim();
    let cleaned = strip_trailing_punct(line);
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

/// Return the first ISSN string (dashed canonical form) found in the
/// footer.
pub fn detect_issn(footer_text: &str) -> Option<String> {
    ISSN_RE.find(footer_text).map(|m| m.as_str().to_string())
}

/// Pick the abstract body.
///
/// PDF inputs go through [`extract_paper_abstract`], which reads the
/// source file again with PDFium and runs the validated anchor /
/// section-marker / fallback policy on its native reading-order text.
/// The general extract adapter's character-level paragraph
/// reconstruction is too lossy on paper layouts — funding footnotes
/// fragment the abstract block, two-column heading separators are
/// merged into the body, and the per-page quality gate rejects some
/// paper PDFs outright — so we bypass it for this single decision.
/// A failure inside the PDF-aware path falls through to the block
/// scan below so a corrupt PDF still records what little can be
/// salvaged.
///
/// All other formats (EPUB, HTML, TXT) use the block-level fallback
/// chain that still works well for those: anchor an `Abstract`
/// keyword inside a body block, then the first long paragraph on the
/// first source unit (page or spine document zero), then the longest
/// body paragraph overall.
pub fn extract_abstract(
    file: &Path,
    extraction: &Extraction,
    _strategy: AbstractStrategy,
) -> Option<(String, &'static str)> {
    if is_pdf_path(file)
        && let Ok(result) = extract_paper_abstract(file)
    {
        return result;
    }

    let bodies: Vec<&Block> = extraction
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Body))
        .collect();

    for (idx, b) in bodies.iter().enumerate() {
        let trimmed = b.text.trim_start();
        let Some(rest) = strip_abstract_prefix(trimmed) else {
            continue;
        };
        let rest = rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        if rest.chars().count() >= 50 {
            return Some((rest.to_string(), "heading"));
        }
        if let Some(next) = bodies
            .iter()
            .skip(idx + 1)
            .find(|nb| !nb.text.trim().is_empty())
        {
            return Some((next.text.clone(), "heading"));
        }
    }

    if let Some(first_unit) = bodies.first().map(|b| b.source_unit)
        && let Some(b) = bodies
            .iter()
            .filter(|b| b.source_unit == first_unit)
            .find(|b| b.text.chars().count() >= 200)
    {
        return Some((b.text.clone(), "first_page_long_para"));
    }

    bodies
        .iter()
        .max_by_key(|b| b.text.chars().count())
        .map(|b| (b.text.clone(), "first_long_para"))
}

fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Concatenate all body block texts with newlines. Used by the DOI
/// scan, which can match anywhere in the paper.
pub fn body_text(extraction: &Extraction) -> String {
    join_blocks(extraction, |_| true)
}

/// Concatenate the body block texts of the final source unit. The
/// arXiv, ISSN, and venue cues live in the masthead or page footer of
/// the last page in published-paper layouts.
pub fn footer_text(extraction: &Extraction) -> String {
    let last_unit = extraction
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Body))
        .map(|b| b.source_unit)
        .max();
    match last_unit {
        Some(unit) => join_blocks(extraction, |b| b.source_unit == unit),
        None => String::new(),
    }
}

fn join_blocks(extraction: &Extraction, mut keep: impl FnMut(&Block) -> bool) -> String {
    extraction
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Body) && keep(b))
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_abstract_prefix(s: &str) -> Option<&str> {
    for kw in ["Abstract", "ABSTRACT", "abstract"] {
        if let Some(rest) = s.strip_prefix(kw) {
            match rest.chars().next() {
                None => return Some(rest),
                Some(c) if c.is_whitespace() || c == ':' || c == '.' => return Some(rest),
                _ => {}
            }
        }
    }
    None
}

fn strip_trailing_punct(s: &str) -> &str {
    s.trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '\'', '"'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_extract::{Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc};

    fn synthetic_extraction(blocks: Vec<(BlockKind, &str, u32)>) -> Extraction {
        Extraction {
            blocks: blocks
                .into_iter()
                .map(|(kind, text, source_unit)| Block {
                    kind,
                    text: text.to_string(),
                    source_unit,
                    style: None,
                })
                .collect(),
            toc: Toc::default(),
            biblio: Default::default(),
            provenance: Provenance {
                adapter: "test".to_string(),
                extractor_version: 0,
                text_layer_quality: TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        }
    }

    #[test]
    fn detect_doi_finds_inline_doi() {
        assert_eq!(
            detect_doi("DOI: 10.5555/synthetic.0001 and more text"),
            Some("10.5555/synthetic.0001".to_string()),
        );
    }

    #[test]
    fn detect_doi_strips_trailing_period() {
        assert_eq!(
            detect_doi("see https://doi.org/10.5555/synthetic.0001."),
            Some("10.5555/synthetic.0001".to_string()),
        );
    }

    #[test]
    fn detect_doi_returns_none_on_no_match() {
        assert_eq!(detect_doi("no identifier here"), None);
    }

    #[test]
    fn detect_arxiv_id_handles_new_form_in_footer() {
        assert_eq!(
            detect_arxiv_id(None, "arXiv:0000.00001 [cs.XX] 1 Jan 2020"),
            Some("0000.00001".to_string()),
        );
    }

    #[test]
    fn detect_arxiv_id_strips_version_suffix() {
        assert_eq!(
            detect_arxiv_id(None, "arXiv:0000.00001v3 [cs.XX]"),
            Some("0000.00001".to_string()),
        );
    }

    #[test]
    fn detect_arxiv_id_prefers_info_title() {
        assert_eq!(
            detect_arxiv_id(Some("0000.00002v1"), "arXiv:0000.00001 [cs.XX]"),
            Some("0000.00002".to_string()),
        );
    }

    #[test]
    fn detect_arxiv_id_handles_old_form() {
        assert_eq!(
            detect_arxiv_id(Some("math.AG/0000000"), ""),
            Some("math.AG/0000000".to_string()),
        );
    }

    #[test]
    fn detect_venue_matches_proceedings_of_line() {
        let footer = "preamble\nProceedings of the Synthetic Conference, 2020.\nmore footer\n";
        assert_eq!(
            detect_venue(footer),
            Some("Proceedings of the Synthetic Conference, 2020".to_string()),
        );
    }

    #[test]
    fn detect_venue_matches_conference_on_phrase() {
        let footer = "Synthetic Conference on Test Spaces, 2020";
        assert_eq!(
            detect_venue(footer),
            Some("Synthetic Conference on Test Spaces, 2020".to_string()),
        );
    }

    #[test]
    fn detect_venue_matches_vol_phrase() {
        let footer = "Synthetic Journal, Vol. 9, No. 3, 2020";
        assert_eq!(
            detect_venue(footer),
            Some("Synthetic Journal, Vol. 9, No. 3, 2020".to_string()),
        );
    }

    #[test]
    fn detect_venue_returns_none_on_plain_text() {
        assert_eq!(detect_venue("just a regular sentence"), None);
    }

    #[test]
    fn detect_issn_matches_dashed_form() {
        assert_eq!(
            detect_issn("ISSN 0000-000X published quarterly"),
            Some("0000-000X".to_string()),
        );
    }

    #[test]
    fn extract_abstract_picks_heading_with_inline_body() {
        let extraction = synthetic_extraction(vec![(
            BlockKind::Body,
            "Abstract: this synthetic abstract body is intentionally long enough.",
            0,
        )]);
        let (text, source) =
            extract_abstract(NON_PDF, &extraction, AbstractStrategy::HeadingFirst).unwrap();
        assert!(text.starts_with("this synthetic abstract"));
        assert_eq!(source, "heading");
    }

    #[test]
    fn extract_abstract_picks_next_block_when_heading_stands_alone() {
        let extraction = synthetic_extraction(vec![
            (BlockKind::Body, "Abstract", 0),
            (
                BlockKind::Body,
                "Synthetic abstract body across multiple sentences.",
                0,
            ),
        ]);
        let (text, source) =
            extract_abstract(NON_PDF, &extraction, AbstractStrategy::HeadingFirst).unwrap();
        assert_eq!(text, "Synthetic abstract body across multiple sentences.");
        assert_eq!(source, "heading");
    }

    #[test]
    fn extract_abstract_falls_back_to_first_page_long_paragraph() {
        let long = "a".repeat(220);
        let extraction = synthetic_extraction(vec![
            (BlockKind::Body, "Short line.", 0),
            (BlockKind::Body, &long, 0),
            (BlockKind::Body, "Body on the next page.", 1),
        ]);
        let (text, source) =
            extract_abstract(NON_PDF, &extraction, AbstractStrategy::HeadingFirst).unwrap();
        assert_eq!(text, long);
        assert_eq!(source, "first_page_long_para");
    }

    #[test]
    fn extract_abstract_falls_back_to_longest_body() {
        let extraction = synthetic_extraction(vec![
            (BlockKind::Body, "Short line.", 0),
            (BlockKind::Body, "Body on the next page.", 1),
            (BlockKind::Body, "The longest body of the document.", 2),
        ]);
        let (text, source) =
            extract_abstract(NON_PDF, &extraction, AbstractStrategy::HeadingFirst).unwrap();
        assert_eq!(text, "The longest body of the document.");
        assert_eq!(source, "first_long_para");
    }

    #[test]
    fn extract_abstract_returns_none_on_empty_extraction() {
        let extraction = synthetic_extraction(vec![]);
        assert!(extract_abstract(NON_PDF, &extraction, AbstractStrategy::HeadingFirst).is_none());
    }

    /// Synthetic non-PDF path used by the block-level test cases above
    /// so the PDF dispatch branch is bypassed.
    const NON_PDF: &Path = unsafe {
        // SAFETY: `Path` is `#[repr(transparent)]` over `OsStr`, and
        // `OsStr` is `#[repr(transparent)]` over `[u8]` on Unix /
        // `[u16]` on Windows. Casting from a `&'static str` to a
        // `&'static Path` round-trips the same bytes. `std` only
        // exposes `Path::new` (returns `&Path`), which is not const,
        // so we shim a const equivalent for this constant. The
        // pointer cast is the only viable path here.
        &*(std::ptr::from_ref::<str>("synthetic.epub") as *const Path)
    };
}
