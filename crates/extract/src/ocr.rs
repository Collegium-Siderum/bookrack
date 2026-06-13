// SPDX-License-Identifier: Apache-2.0

//! `OcrAdapter`: a polyocr single-file Markdown product plus its source
//! PDF → [`Extraction`].
//!
//! # Format commitment
//!
//! The OCR adapter consumes polyocr's Markdown product in either of
//! the two on-disk shapes the tool emits:
//!
//! - **single file** — verbatim file bytes, the form `polyocr` writes
//!   in `stdout` / single-file output mode.
//! - **directory** — a folder of `page_*.md` files, the form
//!   `polyocr book.pdf out_dir/` writes. [`read_source`] reads it as
//!   the canonical concatenation of those files in filename order
//!   joined by a single `\n`. The OCR intake's `source_sha256` and
//!   `byte_size` are computed against that canonical text, so two
//!   ingests of the same directory always register the same intake.
//!
//! Both shapes share `intake.format = "ocr-markdown"`: the format
//! describes the *content shape* (the marker grammar below), not the
//! on-disk container.
//!
//! The page body is broken into pages by
//! `<!-- page <label> (sheet <n>) -->` markers. The marker is the
//! delimiter; everything between consecutive markers is body,
//! attributed to the page's `(sheet n)` count.
//!
//! Optional YAML frontmatter at the head of the file carries advisory
//! provenance (engine name, preset, dpi, …). It is skipped over in this
//! version: its fields are forensic for the user, not consumed by any
//! decision the adapter takes.
//!
//! Pages are not interpreted beyond their body text: every line that is
//! not blank becomes part of a [`BlockKind::Body`] block, paragraphs
//! split on blank lines. There is no heading detection (OCR carries no
//! reliable structural signal) — TOC and biblio come from the source
//! PDF instead.
//!
//! ## Source-PDF coupling
//!
//! When `source_pdf` is `Some`, the adapter re-opens that file to lift
//! `/Outline` (TOC) and `/Info` (biblio). Both are PDF *metadata*-layer
//! constructs that do not depend on the text layer, so this works even
//! when the source PDF's text layer was rejected by the quality gate
//! (the canonical OCR-intake situation). The shared [`crate::pdf`]
//! helpers do the lifting; the OCR adapter only re-opens the file and
//! holds the process-level PDFium lock for the duration of the read.
//!
//! ## Provenance
//!
//! Stamped as `adapter = "ocr-pages"`, `extractor_version =
//! `[`crate::OCR_INTAKE_VERSION`], `text_layer_quality =
//! `[`TextLayerQuality::Doubtful`]. `derived_from_sha256` is taken from
//! the `source_pdf_sha256` parameter verbatim — the caller (ingest)
//! already needs the source PDF's hash to register its intake, and the
//! adapter does not re-hash it.

use std::path::Path;

use crate::OCR_INTAKE_VERSION;
use crate::contract::{
    Biblio, Block, BlockKind, ExtractError, Extraction, Provenance, TextLayerQuality, Toc,
};
use crate::pdf;

/// Adapter string written to `Provenance.adapter` and propagated to
/// `intake.adapter`. Part of the format commitment in
/// [`crate::contract`] and `crates/catalog/src/intake.rs`: once a row
/// carries this value, the binary keeps recognising it forever.
pub const ADAPTER: &str = "ocr-pages";

/// Read the OCR product's canonical text from `ocr_path`, accepting
/// either form polyocr emits:
///
/// - A single file: read verbatim.
/// - A directory: enumerate every regular file named `page_*.md`,
///   sort by filename (so `page_001.md` < `page_002.md`), and join
///   the contents with a single `\n` separator. The "canonical text"
///   for a directory is this concatenation; downstream the OCR
///   intake's `source_sha256` and `byte_size` are computed against
///   it, so two runs against the same directory always register the
///   same intake.
///
/// The two forms commit to the same `intake.format = "ocr-markdown"`:
/// the format describes the *content shape* (polyocr's marker
/// grammar), not the on-disk container.
pub fn read_source(ocr_path: &Path) -> Result<String, ExtractError> {
    if ocr_path.is_dir() {
        read_dir_concat(ocr_path)
    } else {
        Ok(std::fs::read_to_string(ocr_path)?)
    }
}

fn read_dir_concat(dir: &Path) -> Result<String, ExtractError> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("page_") && name.ends_with(".md") {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return Err(ExtractError::MalformedPackage {
            detail: format!("no `page_*.md` files in directory {}", dir.display()),
        });
    }
    paths.sort();
    let mut out = String::new();
    for path in paths {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&std::fs::read_to_string(path)?);
    }
    Ok(out)
}

/// Extract one OCR-intake product into an [`Extraction`].
///
/// `ocr_path` is the polyocr product — a single file, or a directory
/// of `page_*.md` files (see [`read_source`]). `source_pdf`, when
/// `Some`, is opened to lift `/Outline` and `/Info` via the shared PDF
/// helpers; when `None`, TOC and biblio default to empty.
/// `source_pdf_sha256` is recorded verbatim in
/// `Provenance.derived_from_sha256`; computing the hash is the
/// caller's responsibility because the same caller already needs it
/// to register the source PDF intake.
pub fn extract(
    ocr_path: &Path,
    source_pdf: Option<&Path>,
    source_pdf_sha256: Option<&str>,
) -> Result<Extraction, ExtractError> {
    let text = read_source(ocr_path)?;
    extract_from_text(&text, source_pdf, source_pdf_sha256)
}

/// Extract one OCR-intake product whose text is already in memory.
///
/// Same shape as [`extract`], minus the file/dir read step. Lets
/// `ingest` compute the OCR intake's `source_sha256` and `byte_size`
/// over the same canonical text the parser is about to consume,
/// without round-tripping through a temp file.
pub fn extract_from_text(
    text: &str,
    source_pdf: Option<&Path>,
    source_pdf_sha256: Option<&str>,
) -> Result<Extraction, ExtractError> {
    let body = strip_frontmatter(text);
    let pages = scan_pages(body)?;
    let blocks = blocks_from_pages(&pages);
    if !blocks.iter().any(|b| matches!(b.kind, BlockKind::Body)) {
        return Err(ExtractError::EmptyExtraction);
    }
    let (toc, biblio) = match source_pdf {
        Some(p) => read_pdf_metadata(p, &blocks)?,
        None => (Toc::default(), Biblio::default()),
    };

    Ok(Extraction {
        blocks,
        toc,
        biblio,
        provenance: Provenance {
            adapter: ADAPTER.to_string(),
            extractor_version: OCR_INTAKE_VERSION,
            text_layer_quality: TextLayerQuality::Doubtful,
            skipped_units: Vec::new(),
            derived_from_sha256: source_pdf_sha256.map(str::to_string),
            partial_pages: None,
        },
    })
}

/// Strip an optional `---\n…\n---\n` YAML frontmatter prefix. The
/// content is not interpreted: only its bracketing is recognised. An
/// unclosed frontmatter leaves the input untouched so the marker scan
/// can produce a precise diagnostic.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    let Some(end_off) = rest.find("\n---\n") else {
        return text;
    };
    &rest[end_off + "\n---\n".len()..]
}

/// One page extracted from the marker scan, between two consecutive
/// markers (or the last marker and EOF).
#[cfg_attr(test, derive(Debug))]
struct Page {
    /// 1-based physical sheet number from the marker.
    sheet: u32,
    /// Raw body text between this marker and the next.
    body: String,
}

const MARKER_PREFIX: &str = "<!-- page ";
const MARKER_INFIX: &str = " (sheet ";
const MARKER_SUFFIX: &str = ") -->";

/// Walk the body after frontmatter removal, splitting it into one
/// `Page` per `<!-- page <label> (sheet <n>) -->` marker.
fn scan_pages(text: &str) -> Result<Vec<Page>, ExtractError> {
    let Some(first) = text.find(MARKER_PREFIX) else {
        return Err(ExtractError::MalformedPackage {
            detail: "no page markers found".into(),
        });
    };
    let prelude = text[..first].trim();
    if !prelude.is_empty() {
        let head: String = prelude.chars().take(40).collect();
        return Err(ExtractError::MalformedPackage {
            detail: format!("content before the first page marker: {head}"),
        });
    }

    let mut out = Vec::new();
    let mut cursor = first;
    while let Some(prefix_rel) = text[cursor..].find(MARKER_PREFIX) {
        let abs_prefix = cursor + prefix_rel;
        let after_prefix = abs_prefix + MARKER_PREFIX.len();
        let Some(infix_off) = text[after_prefix..].find(MARKER_INFIX) else {
            return Err(ExtractError::MalformedPackage {
                detail: "marker missing `(sheet ` infix".into(),
            });
        };
        let after_infix = after_prefix + infix_off + MARKER_INFIX.len();
        let Some(suffix_off) = text[after_infix..].find(MARKER_SUFFIX) else {
            return Err(ExtractError::MalformedPackage {
                detail: "marker missing `) -->` suffix".into(),
            });
        };
        let sheet_str = text[after_infix..after_infix + suffix_off].trim();
        let sheet: u32 = sheet_str
            .parse()
            .map_err(|_| ExtractError::MalformedPackage {
                detail: format!("marker sheet number not an integer: {sheet_str:?}"),
            })?;
        let body_start = after_infix + suffix_off + MARKER_SUFFIX.len();
        let body_end = text[body_start..]
            .find(MARKER_PREFIX)
            .map(|off| body_start + off)
            .unwrap_or(text.len());
        out.push(Page {
            sheet,
            body: text[body_start..body_end].to_string(),
        });
        cursor = body_end;
    }
    Ok(out)
}

/// Split each page's body into `BlockKind::Body` paragraphs on blank
/// lines, collapsing each paragraph's internal newlines to spaces.
/// `source_unit` is the sheet's 0-based index.
fn blocks_from_pages(pages: &[Page]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for page in pages {
        let source_unit = page.sheet.saturating_sub(1);
        for chunk in page.body.split("\n\n") {
            let paragraph: String = chunk
                .split('\n')
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if paragraph.is_empty() {
                continue;
            }
            blocks.push(Block {
                kind: BlockKind::Body,
                text: paragraph,
                source_unit,
                style: None,
            });
        }
    }
    blocks
}

/// Read the source PDF's physical sheet count. `/Pages` is a PDF
/// metadata-layer construct independent of the text layer, so this
/// works against a scan whose body the quality gate rejected. Callers
/// use the value as the expected page count for the OCR-intake
/// completeness check.
pub fn count_pdf_pages(source_pdf: &Path) -> Result<u32, ExtractError> {
    let pdfium = pdf::pdfium()?;
    let _guard = pdf::EXTRACTION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let document =
        pdfium
            .load_pdf_from_file(source_pdf, None)
            .map_err(|e| ExtractError::CorruptFile {
                detail: e.to_string(),
            })?;
    let len = document.pages().len();
    u32::try_from(len).map_err(|_| ExtractError::CorruptFile {
        detail: format!("page count out of range: {len}"),
    })
}

/// Re-open the source PDF to lift its `/Outline` into a TOC anchored on
/// `blocks` and its `/Info` into a `Biblio`. Both reads use the shared
/// PDFium handle and the process-level extraction lock the PDF adapter
/// declares.
fn read_pdf_metadata(source_pdf: &Path, blocks: &[Block]) -> Result<(Toc, Biblio), ExtractError> {
    let pdfium = pdf::pdfium()?;
    // The lock guards a sequence of PDFium calls; recover from a
    // previous panicked extraction rather than failing every later
    // open, matching the PDF adapter's strategy.
    let _guard = pdf::EXTRACTION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let document =
        pdfium
            .load_pdf_from_file(source_pdf, None)
            .map_err(|e| ExtractError::CorruptFile {
                detail: e.to_string(),
            })?;
    let toc = pdf::build_toc(&document, blocks);
    let biblio = pdf::build_biblio(&pdf::read_info_tags(&document));
    Ok((toc, biblio))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_frontmatter_removes_a_well_formed_block() {
        let text = "---\nschema: 1\nengine: x\n---\n\nbody";
        assert_eq!(strip_frontmatter(text), "\nbody");
    }

    #[test]
    fn strip_frontmatter_leaves_input_untouched_when_unopened() {
        let text = "no frontmatter here\n";
        assert_eq!(strip_frontmatter(text), "no frontmatter here\n");
    }

    #[test]
    fn strip_frontmatter_leaves_input_untouched_when_unclosed() {
        let text = "---\nopen but never closed\n";
        assert_eq!(strip_frontmatter(text), "---\nopen but never closed\n");
    }

    #[test]
    fn scan_pages_splits_on_markers_and_records_sheet_numbers() {
        let text = "\
<!-- page 1 (sheet 1) -->

first page body

<!-- page iii (sheet 2) -->

second page body across
two source lines

<!-- page 12 (sheet 3) -->

third page body
";
        let pages = scan_pages(text).expect("scan");
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].sheet, 1);
        assert_eq!(pages[1].sheet, 2);
        assert_eq!(pages[2].sheet, 3);
        assert!(pages[0].body.contains("first page body"));
        assert!(pages[1].body.contains("two source lines"));
    }

    #[test]
    fn scan_pages_rejects_content_before_the_first_marker() {
        let text = "an unexpected preamble\n\n<!-- page 1 (sheet 1) -->\n\nbody\n";
        let err = scan_pages(text).expect_err("must reject preamble");
        assert!(matches!(err, ExtractError::MalformedPackage { .. }));
    }

    #[test]
    fn scan_pages_rejects_a_malformed_marker() {
        let text = "<!-- page 1 (sheet abc) -->\n\nbody\n";
        let err = scan_pages(text).expect_err("must reject non-integer sheet");
        assert!(matches!(err, ExtractError::MalformedPackage { .. }));
    }

    #[test]
    fn read_source_concatenates_a_polyocr_directory_in_filename_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Write four files; one outside the page_* pattern stays out
        // of the concat, and the kept files end up in numeric order.
        std::fs::write(
            dir.path().join("page_002.md"),
            "<!-- page 2 (sheet 2) -->\n\nbody two\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("page_001.md"),
            "<!-- page 1 (sheet 1) -->\n\nbody one\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("page_010.md"),
            "<!-- page 10 (sheet 10) -->\n\nbody ten\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "ignore this\n").unwrap();

        let text = read_source(dir.path()).expect("read dir");
        // ASCII sort: page_001 < page_002 < page_010 ≪ README.
        let body_positions = [
            text.find("body one").expect("page 1 in concat"),
            text.find("body two").expect("page 2 in concat"),
            text.find("body ten").expect("page 10 in concat"),
        ];
        assert!(body_positions.windows(2).all(|w| w[0] < w[1]));
        assert!(
            !text.contains("ignore this"),
            "README.md must not enter the concat",
        );
    }

    #[test]
    fn read_source_rejects_an_empty_polyocr_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No page_*.md files in the directory.
        std::fs::write(dir.path().join("README.md"), "ignore\n").unwrap();
        let err = read_source(dir.path()).expect_err("must reject empty dir");
        assert!(matches!(err, ExtractError::MalformedPackage { .. }));
    }

    #[test]
    fn extract_from_text_matches_extract_when_text_round_trips_via_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sample.md");
        let text =
            "<!-- page 1 (sheet 1) -->\n\nbody one\n\n<!-- page 2 (sheet 2) -->\n\nbody two\n";
        std::fs::write(&path, text).unwrap();

        let from_file = extract(&path, None, None).expect("extract file");
        let from_text = extract_from_text(text, None, None).expect("extract text");
        assert_eq!(from_file, from_text);
    }

    #[test]
    fn blocks_from_pages_join_internal_newlines_within_a_paragraph() {
        let pages = vec![
            Page {
                sheet: 1,
                body: "\nfirst para line one\nfirst para line two\n\nsecond para\n".into(),
            },
            Page {
                sheet: 2,
                body: "\nthird para\n".into(),
            },
        ];
        let blocks = blocks_from_pages(&pages);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].source_unit, 0);
        assert_eq!(blocks[0].text, "first para line one first para line two");
        assert_eq!(blocks[1].source_unit, 0);
        assert_eq!(blocks[1].text, "second para");
        assert_eq!(blocks[2].source_unit, 1);
        assert_eq!(blocks[2].text, "third para");
    }
}
