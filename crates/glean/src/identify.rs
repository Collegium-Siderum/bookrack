// SPDX-License-Identifier: Apache-2.0

//! Local IDENTIFY pass for the glean pipeline. Pure pattern matching
//! over the extracted text and the source filename — no network
//! calls. Each detector returns the canonical form (no `arXiv:`
//! prefix, no `DOI:` prefix, no trailing sentence punctuation) so
//! callers can write the result straight into the catalog.
//!
//! Input shape:
//!
//! * `metadata_text` — the fullwidth-folded, references-truncated
//!   raw page text from
//!   [`bookrack_extract::extract_paper_metadata_text`]. Empty for
//!   non-PDF inputs.
//! * `filename_stem` — the source file stem. DOI- and arXiv-encoded
//!   names (`10.18654_1000-0569_2025.04.17.pdf`, `arxiv-1706.03762.pdf`,
//!   `N19-1423.pdf`, `RJ-2016-007.pdf`) are folded to canonical
//!   identifiers and used in preference to text scans, since curator
//!   naming is more reliable than character-level PDF text recovery
//!   for the few publishers that drop or split the identifier in the
//!   visible layer (ACL Anthology, R Journal, Acta Petrologica
//!   Sinica fullwidth glyphs).
//! * `info_title` — the PDF `/Info /Title` field. Used only as a
//!   secondary source for the arXiv id (old-form ids are sometimes
//!   stamped there) after a sniff filter rejects template filenames
//!   and stray arXiv banners.

use std::path::Path;
use std::sync::LazyLock;

use bookrack_extract::{Biblio, Block, BlockKind, Extraction, extract_paper_abstract};
use regex::Regex;

use crate::AbstractStrategy;

// ─── DOI ─────────────────────────────────────────────────────────────

/// DOI shape (CrossRef syntax): the registrant prefix
/// `10.NNNN[NNNNN]` joined by `/` to a suffix over the conservative
/// DOI character set.
static DOI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"10\.\d{4,9}/[-._;()/:A-Za-z0-9]+").expect("DOI regex"));

static DOI_LOOSE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // After the first run of DOI-class characters, every continuation
    // run must start with a digit, dot, slash, or hyphen — anything
    // that the PDF kerning artefact could plausibly produce, but never
    // a letter that begins a fresh word ("and more text"). The
    // restriction stops the loose match from sliding into surrounding
    // prose; the collapsed candidate is still re-validated against
    // [`DOI_RE`] before acceptance.
    Regex::new(r"10\s*\.\s*\d{4,9}\s*/\s*[-._;():A-Za-z0-9]+(?:\s+[\d./\-][-._;():A-Za-z0-9]*)*")
        .expect("loose DOI regex")
});

/// DOI placeholders — camera-ready templates whose author never set
/// the assigned identifier. ACM produces `10.1145/nnnnnnn.nnnnnnn`;
/// other styles use `XXXX` / `YYYY.MM`.
static DOI_PLACEHOLDER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)/[nNxX]{3,}|/[Yy]{4}\.[Mm]{2}|nnnnnnn|XXXXXX|0000-0000")
        .expect("DOI placeholder regex")
});

/// Filename DOI rules. The cache convention encodes the DOI as the
/// file stem with `/` folded to `_`; the suffix may also contain `_`
/// where the original DOI had `/` (Acta Petrologica Sinica's
/// `10.18654/1000-0569/2025.04.17`).
static FN_DOI_NUMERIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(10\.\d{4,9})_(.+)$").expect("filename DOI regex"));

/// ACL Anthology codes (`N19-1423`, `D21-1234`, ...) map to
/// `10.18653/v1/<lowercase>` per the publisher's DOI scheme.
static FN_ACL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?i)[A-Z]\d{2}-\d{3,4}$").expect("ACL anthology regex"));

/// R Journal codes (`RJ-2016-007`) map to `10.32614/<lowercase>` per
/// the publisher's DOI scheme.
static FN_RJ_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?i)RJ-\d{4}-\d{3,4}$").expect("R Journal regex"));

// ─── arXiv ───────────────────────────────────────────────────────────

/// arXiv new-form identifier (April 2007 onwards) immediately
/// preceded by an `arXiv:` token. The prefix requirement prevents
/// matching `NNNN.NNNNN`-shaped numbers that appear naturally in the
/// references section (proceedings IDs, page ranges).
static ARXIV_NEW_PREFIXED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)arxiv[:\s]+(\d{4}\.\d{4,5})(?:v\d+)?").expect("arXiv new prefixed regex")
});

/// arXiv new-form identifier immediately followed by a subject
/// bracket — covers the page-banner shape `1706.03762 [cs.CL]` that
/// arXiv PDFs print without the `arXiv:` prefix repeated.
static ARXIV_NEW_BRACKET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(\d{4}\.\d{4,5})(?:v\d+)?\s*\[(?:cs|math|physics|q-bio|q-fin|stat|eess|econ|astro-ph|cond-mat|gr-qc|hep-[a-z]+|math-ph|nlin|nucl-[a-z]+|quant-ph)",
    )
    .expect("arXiv new bracket regex")
});

/// arXiv old-form identifier (pre-2007) `cat[.sub]/NNNNNNN` with the
/// `arXiv:` prefix required.
static ARXIV_OLD_PREFIXED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)arxiv[:\s]+([a-z][a-z\-]*(?:\.[A-Za-z\-]+)?/\d{7})(?:v\d+)?")
        .expect("arXiv old prefixed regex")
});

/// arXiv old-form identifier without the `arXiv:` prefix. Only
/// applied to `info_title`, where the banner is the entire string and
/// false positives over body DOI suffixes cannot occur.
static ARXIV_OLD_RAW_IN_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([a-z][a-z\-]*(?:\.[A-Za-z\-]+)?/\d{7})(?:v\d+)?\b")
        .expect("arXiv old raw regex")
});

/// Filename rule for new-form arXiv IDs (`arxiv-1706.03762.pdf`).
static FN_ARXIV_NEW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^arxiv-(\d{4}\.\d{4,5})$").expect("filename arXiv new regex"));

/// Filename rule for old-form arXiv IDs (`arxiv-math_0211159.pdf`)
/// — the `/` is escaped to `_` in the on-disk name.
static FN_ARXIV_OLD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^arxiv-([a-z][a-z\-]*)_(\d{7})$").expect("filename arXiv old regex")
});

/// Canonical form of new-form arXiv IDs (`NNNN.NNNNN`); capture
/// groups 1 and 2 isolate the year prefix.
static ARXIV_NEW_FMT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{2})\d{2}\.\d{4,5}$").expect("arXiv year-new regex"));

/// Canonical form of old-form arXiv IDs; capture group 1 isolates
/// the year prefix.
static ARXIV_OLD_FMT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z\-\.]*/(\d{2})\d{5}$").expect("arXiv year-old regex"));

// ─── ISSN / venue / year ─────────────────────────────────────────────

/// ISSN: four digits, dash, three digits, then a check character
/// (digit or `X`).
static ISSN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{4}-\d{3}[\dX]\b").expect("ISSN regex"));

/// Venue cue phrases. A match seeds a line-level lookup that returns
/// the whole line as the venue string.
static VENUE_CUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\bproceedings of\b|\bconference on\b|\bvol\.)").expect("venue cue regex")
});

/// Maximum character count of a venue line accepted by
/// [`detect_venue`]. Above this the line is almost always a bibliography
/// entry that swallowed the cue word.
const MAX_VENUE_CHARS: usize = 120;

static D_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^D:[0-9]{8,}").expect("PDF date regex"));

/// Year embedded in a DOI suffix between slashes, hyphens, or dots
/// (`10.1038/s41467-2024-NNNN`, `10.1360/csb-2025-0635`,
/// `10.18654/1000-0569/2025.04.17`).
static DOI_YEAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\-|/|\.)(20\d{2}|19\d{2})(?:\-|/|\.)").expect("DOI-embedded year regex")
});

/// Copyright stamp `(c) YYYY` / `© YYYY` / literal `copyright YYYY` on
/// the first page.
static COPYRIGHT_YEAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:\u{00A9}|\(c\)|copyright)\s*(20\d{2}|19\d{2})").expect("copyright year regex")
});

/// `Vol. NN, YYYY` masthead pattern common to journal page footers.
static VOL_YEAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)vol\.?\s*\d+[,\s]+(20\d{2}|19\d{2})").expect("vol year regex")
});

// ─── title sniff ─────────────────────────────────────────────────────

/// Production source-filename extensions that show up in `/Info.Title`
/// when the camera-ready PDF was rendered straight from the typesetter
/// (`PLME0208_696-701.indd`, `paper.tex`).
static TITLE_FILENAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(indd|docx?|tex|qxd|qxp|pages|ppt|key)\b")
        .expect("title-filename sniff regex")
});

/// Minimum alphabetic-character ratio for a string to be accepted as
/// a title. Production filenames like `CSB-2025-0635-online 1..13`
/// land well below this cap. Pure-CJK titles score 1.0 since
/// `char::is_alphabetic` recognises ideographs as letters.
const TITLE_MIN_ALPHA_RATIO: f32 = 0.55;

// ─── public detectors ────────────────────────────────────────────────

/// DOI of the source paper. Filename-derived DOIs are preferred over
/// text matches; text matches are validated against the placeholder
/// list before being returned.
pub fn detect_doi(metadata_text: Option<&str>, filename_stem: Option<&str>) -> Option<String> {
    if let Some(stem) = filename_stem
        && let Some(doi) = doi_from_filename(stem)
    {
        return Some(doi);
    }
    metadata_text.and_then(doi_from_text)
}

/// arXiv identifier of the source paper in canonical form (no
/// `arXiv:` prefix, no version suffix). Filename first, then the PDF
/// `/Info /Title`, then the metadata text with an `arXiv:` prefix or
/// subject bracket required.
pub fn detect_arxiv_id(
    info_title: Option<&str>,
    metadata_text: Option<&str>,
    filename_stem: Option<&str>,
) -> Option<String> {
    if let Some(stem) = filename_stem
        && let Some(id) = arxiv_from_filename(stem)
    {
        return Some(id);
    }
    if let Some(title) = info_title
        && let Some(id) = arxiv_from_info_title(title)
    {
        return Some(id);
    }
    metadata_text.and_then(arxiv_from_text)
}

/// Return the line containing the first venue cue, when the line is
/// short enough to be a masthead string and not a citation that
/// happened to include a cue word.
pub fn detect_venue(metadata_text: Option<&str>) -> Option<String> {
    let text = metadata_text?;
    let cue = VENUE_CUE_RE.find(text)?;
    let line_start = text[..cue.start()].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[cue.end()..]
        .find('\n')
        .map_or(text.len(), |i| cue.end() + i);
    let line = text[line_start..line_end].trim();
    if line.chars().count() > MAX_VENUE_CHARS {
        return None;
    }
    let cleaned = strip_trailing_punct(line);
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

/// Return the first ISSN string (dashed canonical form) in the
/// metadata text.
pub fn detect_issn(metadata_text: Option<&str>) -> Option<String> {
    metadata_text
        .and_then(|t| ISSN_RE.find(t))
        .map(|m| m.as_str().to_string())
}

/// Publication year, picked from the first reliable signal.
///
/// Source order:
///
/// 1. The four-digit prefix of an arXiv id (`1706.03762` → 2017,
///    `math/0211159` → 2002).
/// 2. A four-digit year encoded in a DOI suffix between separators.
/// 3. A copyright stamp (`© YYYY`) or `Vol. NN, YYYY` line in
///    `metadata_text`.
/// 4. `biblio_year`, unless `biblio_year_raw` is the PDF
///    `/Info /CreationDate` shape — that field is the file generation
///    timestamp, not the publication year, and drifts by years on
///    republished archives.
pub fn detect_year(
    arxiv_id: Option<&str>,
    doi: Option<&str>,
    biblio_year: Option<i32>,
    biblio_year_raw: Option<&str>,
    metadata_text: Option<&str>,
) -> Option<i32> {
    if let Some(id) = arxiv_id
        && let Some(y) = year_from_arxiv(id)
    {
        return Some(y);
    }
    if let Some(d) = doi
        && let Some(y) = year_from_doi(d)
    {
        return Some(y);
    }
    if let Some(t) = metadata_text
        && let Some(y) = year_from_text(t)
    {
        return Some(y);
    }
    let raw_is_creationdate = biblio_year_raw.is_some_and(|s| D_DATE_RE.is_match(s));
    if raw_is_creationdate {
        return None;
    }
    biblio_year
}

/// Sniff filter for the PDF `/Info /Title` field. Returns the trimmed
/// title when it looks like an authored paper title, `None` when it
/// looks like a production source filename, a rotated arXiv banner,
/// or a print-management code.
pub fn sniff_title(info_title: Option<&str>) -> Option<String> {
    let raw = info_title?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if TITLE_FILENAME_RE.is_match(trimmed) {
        return None;
    }
    let lower = trimmed.to_lowercase();
    if lower.starts_with("arxiv:") || lower.starts_with("arxiv ") {
        return None;
    }
    let non_ws: Vec<char> = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if non_ws.is_empty() {
        return None;
    }
    let alpha = non_ws.iter().filter(|c| c.is_alphabetic()).count();
    let ratio = alpha as f32 / non_ws.len() as f32;
    if ratio < TITLE_MIN_ALPHA_RATIO {
        return None;
    }
    Some(trimmed.to_string())
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

/// Convenience wrapper: pull the fields [`detect_year`] needs out of a
/// [`Biblio`] so the caller does not have to spell them out at every
/// call site.
pub fn detect_year_from_biblio(
    arxiv_id: Option<&str>,
    doi: Option<&str>,
    biblio: &Biblio,
    metadata_text: Option<&str>,
) -> Option<i32> {
    detect_year(
        arxiv_id,
        doi,
        biblio.year,
        biblio.year_raw.as_deref(),
        metadata_text,
    )
}

// ─── filename rules ──────────────────────────────────────────────────

fn doi_from_filename(stem: &str) -> Option<String> {
    if let Some(c) = FN_DOI_NUMERIC_RE.captures(stem) {
        let prefix = c.get(1)?.as_str().to_string();
        let rest = c.get(2)?.as_str().replace('_', "/");
        return Some(format!("{prefix}/{rest}"));
    }
    if FN_ACL_RE.is_match(stem) {
        return Some(format!("10.18653/v1/{}", stem.to_lowercase()));
    }
    if FN_RJ_RE.is_match(stem) {
        return Some(format!("10.32614/{}", stem.to_lowercase()));
    }
    if let Some(c) = FN_ARXIV_NEW_RE.captures(stem) {
        return Some(format!("10.48550/arxiv.{}", c.get(1)?.as_str()));
    }
    None
}

fn arxiv_from_filename(stem: &str) -> Option<String> {
    if let Some(c) = FN_ARXIV_NEW_RE.captures(stem) {
        return Some(c.get(1)?.as_str().to_string());
    }
    if let Some(c) = FN_ARXIV_OLD_RE.captures(stem) {
        let cat = c.get(1)?.as_str();
        let num = c.get(2)?.as_str();
        return Some(format!("{cat}/{num}"));
    }
    None
}

// ─── text-scan helpers ───────────────────────────────────────────────

fn doi_from_text(text: &str) -> Option<String> {
    for m in DOI_RE.find_iter(text) {
        let candidate = strip_trailing_punct(m.as_str()).to_string();
        if !DOI_PLACEHOLDER_RE.is_match(&candidate) {
            return Some(candidate);
        }
    }
    for m in DOI_LOOSE_RE.find_iter(text) {
        let collapsed: String = m.as_str().chars().filter(|c| !c.is_whitespace()).collect();
        if let Some(strict) = DOI_RE.find(&collapsed) {
            let candidate = strip_trailing_punct(strict.as_str()).to_string();
            if !DOI_PLACEHOLDER_RE.is_match(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn arxiv_from_info_title(title: &str) -> Option<String> {
    if let Some(c) = ARXIV_NEW_PREFIXED_RE.captures(title) {
        return Some(c.get(1)?.as_str().to_string());
    }
    if let Some(c) = ARXIV_OLD_PREFIXED_RE.captures(title) {
        return Some(c.get(1)?.as_str().to_string());
    }
    if let Some(c) = ARXIV_OLD_RAW_IN_TITLE_RE.captures(title) {
        return Some(c.get(1)?.as_str().to_string());
    }
    None
}

fn arxiv_from_text(text: &str) -> Option<String> {
    if let Some(c) = ARXIV_NEW_PREFIXED_RE.captures(text) {
        return Some(c.get(1)?.as_str().to_string());
    }
    if let Some(c) = ARXIV_NEW_BRACKET_RE.captures(text) {
        return Some(c.get(1)?.as_str().to_string());
    }
    if let Some(c) = ARXIV_OLD_PREFIXED_RE.captures(text) {
        return Some(c.get(1)?.as_str().to_string());
    }
    None
}

fn year_from_arxiv(arxiv_id: &str) -> Option<i32> {
    if let Some(c) = ARXIV_NEW_FMT_RE.captures(arxiv_id) {
        let yy: i32 = c.get(1)?.as_str().parse().ok()?;
        return Some(2000 + yy);
    }
    if let Some(c) = ARXIV_OLD_FMT_RE.captures(arxiv_id) {
        let yy: i32 = c.get(1)?.as_str().parse().ok()?;
        return Some(if yy >= 91 { 1900 + yy } else { 2000 + yy });
    }
    None
}

fn year_from_doi(doi: &str) -> Option<i32> {
    DOI_YEAR_RE
        .captures(doi)
        .and_then(|c| c.get(1)?.as_str().parse().ok())
}

fn year_from_text(text: &str) -> Option<i32> {
    let lower = text.to_lowercase();
    for re in [&*COPYRIGHT_YEAR_RE, &*VOL_YEAR_RE] {
        if let Some(c) = re.captures(&lower)
            && let Ok(y) = c.get(1)?.as_str().parse::<i32>()
        {
            return Some(y);
        }
    }
    None
}

fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
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
                source_of_structure: None,
            },
        }
    }

    // ─── DOI ──────────────────────────────────────────────────────

    #[test]
    fn detect_doi_finds_inline_doi() {
        assert_eq!(
            detect_doi(Some("DOI: 10.5555/synthetic.0001 and more text"), None),
            Some("10.5555/synthetic.0001".to_string()),
        );
    }

    #[test]
    fn detect_doi_strips_trailing_period() {
        assert_eq!(
            detect_doi(Some("see https://doi.org/10.5555/synthetic.0001."), None),
            Some("10.5555/synthetic.0001".to_string()),
        );
    }

    #[test]
    fn detect_doi_returns_none_on_no_match() {
        assert_eq!(detect_doi(Some("no identifier here"), None), None);
    }

    #[test]
    fn detect_doi_rejects_acm_camera_ready_placeholder() {
        assert_eq!(
            detect_doi(
                Some("camera-ready DOI: 10.1145/nnnnnnn.nnnnnnn for the proceedings"),
                None,
            ),
            None,
        );
    }

    #[test]
    fn detect_doi_collapses_internal_spaces_from_pdf_text() {
        assert_eq!(
            detect_doi(
                Some("doi:10. 5555 / synthetic. 0042 / 2024. 03. 11 and more text"),
                None,
            ),
            Some("10.5555/synthetic.0042/2024.03.11".to_string()),
        );
    }

    #[test]
    fn detect_doi_prefers_filename_over_text() {
        assert_eq!(
            detect_doi(
                Some("body text mentions 10.9999/wrong.id as a citation"),
                Some("10.5555_synthetic.0001"),
            ),
            Some("10.5555/synthetic.0001".to_string()),
        );
    }

    #[test]
    fn detect_doi_filename_folds_remaining_underscores_to_slashes() {
        assert_eq!(
            detect_doi(None, Some("10.5555_a-b_2025.04.17")),
            Some("10.5555/a-b/2025.04.17".to_string()),
        );
    }

    #[test]
    fn detect_doi_filename_handles_acl_anthology_code() {
        assert_eq!(
            detect_doi(None, Some("N19-1423")),
            Some("10.18653/v1/n19-1423".to_string()),
        );
    }

    #[test]
    fn detect_doi_filename_handles_r_journal_code() {
        assert_eq!(
            detect_doi(None, Some("RJ-2016-007")),
            Some("10.32614/rj-2016-007".to_string()),
        );
    }

    #[test]
    fn detect_doi_filename_handles_arxiv_new() {
        assert_eq!(
            detect_doi(None, Some("arxiv-1706.03762")),
            Some("10.48550/arxiv.1706.03762".to_string()),
        );
    }

    // ─── arXiv ────────────────────────────────────────────────────

    #[test]
    fn detect_arxiv_id_requires_prefix_or_bracket_in_text() {
        assert_eq!(
            detect_arxiv_id(None, Some("see 1808.04444 in the references"), None),
            None,
        );
    }

    #[test]
    fn detect_arxiv_id_matches_arxiv_prefix() {
        assert_eq!(
            detect_arxiv_id(None, Some("arXiv:0000.00001 [cs.XX]"), None),
            Some("0000.00001".to_string()),
        );
    }

    #[test]
    fn detect_arxiv_id_matches_subject_bracket() {
        assert_eq!(
            detect_arxiv_id(None, Some("1706.03762 [cs.CL] page header"), None),
            Some("1706.03762".to_string()),
        );
    }

    #[test]
    fn detect_arxiv_id_strips_version_suffix() {
        assert_eq!(
            detect_arxiv_id(None, Some("arXiv:0000.00001v3 [cs.XX]"), None),
            Some("0000.00001".to_string()),
        );
    }

    #[test]
    fn detect_arxiv_id_old_form_in_info_title() {
        assert_eq!(
            detect_arxiv_id(Some("math/0000000v1 [math.DG]"), None, None),
            Some("math/0000000".to_string()),
        );
    }

    #[test]
    fn detect_arxiv_id_filename_handles_new_form() {
        assert_eq!(
            detect_arxiv_id(None, None, Some("arxiv-1706.03762")),
            Some("1706.03762".to_string()),
        );
    }

    #[test]
    fn detect_arxiv_id_filename_handles_old_form() {
        assert_eq!(
            detect_arxiv_id(None, None, Some("arxiv-math_0211159")),
            Some("math/0211159".to_string()),
        );
    }

    // ─── venue / ISSN ─────────────────────────────────────────────

    #[test]
    fn detect_venue_matches_proceedings_of_line() {
        let text = "preamble\nProceedings of the Synthetic Conference, 2020.\nmore footer\n";
        assert_eq!(
            detect_venue(Some(text)),
            Some("Proceedings of the Synthetic Conference, 2020".to_string()),
        );
    }

    #[test]
    fn detect_venue_returns_none_when_line_is_too_long() {
        let long_line = "x".repeat(MAX_VENUE_CHARS + 1) + " conference on synthetic";
        assert_eq!(detect_venue(Some(&long_line)), None);
    }

    #[test]
    fn detect_venue_returns_none_on_plain_text() {
        assert_eq!(detect_venue(Some("just a regular sentence")), None);
    }

    #[test]
    fn detect_issn_matches_dashed_form() {
        assert_eq!(
            detect_issn(Some("ISSN 0000-000X published quarterly")),
            Some("0000-000X".to_string()),
        );
    }

    // ─── year ─────────────────────────────────────────────────────

    #[test]
    fn detect_year_uses_arxiv_id_first() {
        assert_eq!(
            detect_year(
                Some("2504.13684"),
                None,
                Some(1999),
                Some("D:19990101"),
                None
            ),
            Some(2025),
        );
    }

    #[test]
    fn detect_year_recovers_old_arxiv_year() {
        assert_eq!(
            detect_year(Some("math/0211159"), None, None, None, None),
            Some(2002),
        );
    }

    #[test]
    fn detect_year_uses_doi_year_when_arxiv_absent() {
        assert_eq!(
            detect_year(
                None,
                Some("10.18654/1000-0569/2025.04.17"),
                None,
                Some("D:20240101"),
                None,
            ),
            Some(2025),
        );
    }

    #[test]
    fn detect_year_uses_copyright_stamp_when_id_signals_absent() {
        assert_eq!(
            detect_year(
                None,
                None,
                Some(2099),
                Some("D:20991231"),
                Some("masthead\n\u{00A9} 2017 Synthetic Society of Test\n"),
            ),
            Some(2017),
        );
    }

    #[test]
    fn detect_year_keeps_biblio_year_when_year_raw_is_not_creationdate() {
        assert_eq!(
            detect_year(None, None, Some(2020), Some("2020"), None),
            Some(2020),
        );
    }

    #[test]
    fn detect_year_rejects_biblio_year_when_year_raw_is_creationdate() {
        assert_eq!(
            detect_year(None, None, Some(2099), Some("D:20991231"), None),
            None,
        );
    }

    // ─── title sniff ──────────────────────────────────────────────

    #[test]
    fn sniff_title_accepts_authored_title() {
        assert_eq!(
            sniff_title(Some("Synthetic Methods for Testing Identifier Recovery")),
            Some("Synthetic Methods for Testing Identifier Recovery".to_string()),
        );
    }

    #[test]
    fn sniff_title_rejects_indesign_source_filename() {
        assert_eq!(sniff_title(Some("PLME0208_696-701.indd")), None);
    }

    #[test]
    fn sniff_title_rejects_arxiv_banner_in_title_field() {
        assert_eq!(
            sniff_title(Some("arXiv:0000.00001v1 [cs.XX] 1 Jan 2020")),
            None,
        );
    }

    #[test]
    fn sniff_title_rejects_print_management_code() {
        assert_eq!(sniff_title(Some("CSB-2025-0635-online 1..13")), None);
    }

    #[test]
    fn sniff_title_accepts_pure_cjk_title() {
        // CJK ideographs are alphabetic, so the alpha-ratio gate
        // passes. The literal here is embedded as `\u{...}` escapes so
        // this source file carries no raw CJK bytes.
        let cjk = "\u{878D}\u{5408}\u{65F6}\u{7A7A}\u{57DF}\u{7279}\u{5F81}";
        assert_eq!(sniff_title(Some(cjk)), Some(cjk.to_string()));
    }

    // ─── abstract ─────────────────────────────────────────────────

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
