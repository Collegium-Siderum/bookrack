// SPDX-License-Identifier: Apache-2.0

//! `PdfAdapter`: a PDF's text layer → [`ExtractOutcome`].
//!
//! Unlike the born-digital adapters, a PDF may carry no usable text
//! layer at all — a bare scan, or a text layer too corrupt to trust.
//! Extraction is therefore conditional: text is pulled page by page,
//! its quality assessed (see [`crate::quality`]), and only a usable
//! layer becomes an [`Extraction`] — otherwise the file is routed to
//! OCR via [`ExtractOutcome::NeedsOcr`].
//!
//! Paragraphs are reconstructed from glyph coordinates: text segments
//! are grouped into rows, columns are detected by the gutter between
//! them, and each column's lines are split into paragraphs by spacing
//! and first-line indentation. A full-width element above a two-column
//! body reads before the columns; running headers, footers, and page
//! numbers are dropped.
//!
//! Beyond the body text the adapter lifts the PDF outline (`/Outline`)
//! into a [`Toc`], anchored to blocks by target page, and the `/Info`
//! dictionary into a [`Biblio`].
//!
//! ## Thread safety
//!
//! PDFium's C API is not thread-safe. The `pdfium-render` `thread_safe`
//! feature guards each individual PDFium call with a global mutex, but
//! that is not enough on its own: one extraction is a *sequence* of
//! calls against a stateful document, and letting two sequences
//! interleave corrupts PDFium's internal state (observed as a hard
//! crash). So [`extract`] additionally holds [`EXTRACTION_LOCK`] for
//! its whole body, admitting one extraction into PDFium at a time.
//!
//! The consequence for callers: [`crate::extract`] is safe to call
//! concurrently from many threads, but PDF extraction does not run in
//! parallel with itself — concurrent PDF extractions queue behind one
//! another. EPUB / HTML / TXT extraction touches no PDFium and stays
//! genuinely parallel.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use pdfium_render::prelude::*;

use crate::contract::{
    Biblio, Block, BlockKind, Contributor, ContributorRole, ExtractError, ExtractOutcome,
    Extraction, Provenance, SkippedUnit, Toc, TocEntry,
};
use crate::quality::{self, QualityDecision};

/// Behaviour-sensitive version of this adapter. Bump when a change here
/// shifts block boundaries or the extraction outcome.
const PDF_ADAPTER_VERSION: u32 = 1;

/// Bump on any change to coordinate paragraph reconstruction: it
/// decides block boundaries, so a change must re-extract downstream.
const COORDS_VERSION: u32 = 1;

/// The pinned PDFium native build (see `PDFIUM_VERSION.md`). A different
/// PDFium build can extract text differently, so the build number is
/// part of `extractor_version`. It is a compile-time constant because
/// P0 pins the `pdfium-render` cargo feature to exactly one build —
/// there is nothing to query at runtime.
const PDFIUM_BUILD: u32 = 7763;

/// The process-wide PDFium handle.
///
/// PDFium is loaded once and shared: under the `thread_safe` feature a
/// single `Pdfium` serves every thread, and binding the native library
/// repeatedly would be wasteful. The stored `Result` keeps a failed
/// load from being retried on every call and lets the failure surface
/// as an ordinary `ExtractError` (see [`pdfium`]).
static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

/// Serializes whole PDF extractions against PDFium — see the module's
/// thread-safety note. It guards only a sequence of PDFium calls, never
/// any data, hence `Mutex<()>`.
static EXTRACTION_LOCK: Mutex<()> = Mutex::new(());

/// Extract one PDF file.
pub fn extract(path: &Path) -> Result<ExtractOutcome, ExtractError> {
    let pdfium = pdfium()?;
    // Hold PDFium for the whole extraction (see the module's thread-
    // safety note). A poisoned lock means a previous extraction
    // panicked; recover it rather than failing every later PDF, since
    // the panic was in that call, not a corruption of this guard.
    let _guard = EXTRACTION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let document = match pdfium.load_pdf_from_file(path, None) {
        Ok(document) => document,
        // A user password is required, or the security handler refused
        // the open: this is an encrypted file, not a damaged one.
        Err(PdfiumError::PdfiumLibraryInternalError(
            PdfiumInternalError::PasswordError | PdfiumInternalError::SecurityError,
        )) => return Err(ExtractError::DrmProtected),
        Err(e) => {
            return Err(ExtractError::CorruptFile {
                detail: e.to_string(),
            });
        }
    };

    // Reconstruct each page's paragraphs from glyph coordinates. A page
    // whose text pdfium cannot read is skipped and recorded — not fatal:
    // the rest of the book is still worth extracting (see the
    // `ExtractError` contract). Each kept page keeps its own index, so a
    // block's `source_unit` stays the true page number even when an
    // earlier page was skipped.
    let mut pages_text: Vec<String> = Vec::new();
    let mut pages: Vec<PageParagraphs> = Vec::new();
    let mut skipped_units: Vec<SkippedUnit> = Vec::new();
    let mut image_pages = 0usize;
    for (index, page) in document.pages().iter().enumerate() {
        let index = index as u32;
        match page.text() {
            Ok(text) => {
                if page
                    .objects()
                    .iter()
                    .any(|object| object.object_type() == PdfPageObjectType::Image)
                {
                    image_pages += 1;
                }
                let paragraphs = reconstruct_by_coords(&text, page.width().value);
                pages_text.push(text.all());
                pages.push(PageParagraphs {
                    page: index,
                    paragraphs,
                });
            }
            Err(e) => skipped_units.push(SkippedUnit {
                index,
                reason: e.to_string(),
            }),
        }
    }

    // The quality gate decides extract-vs-OCR. A layer it rejects never
    // becomes an `Extraction`.
    let report = quality::assess(&pages_text, image_pages);
    let grade = match report.verdict {
        QualityDecision::RouteToOcr { reason } => {
            return Ok(ExtractOutcome::NeedsOcr { reason });
        }
        QualityDecision::Keep { grade, .. } => grade,
    };

    let blocks = build_blocks(pages);

    let toc = build_toc(&document, &blocks);
    let biblio = build_biblio(&read_info_tags(&document));

    Ok(ExtractOutcome::Extracted(Extraction {
        blocks,
        toc,
        biblio,
        provenance: Provenance {
            adapter: "pdf".to_string(),
            extractor_version: extractor_version(),
            text_layer_quality: grade,
            skipped_units,
        },
    }))
}

/// The behaviour-sensitive version stamp for the PDF adapter.
///
/// It concatenates every dimension a downstream re-extraction must
/// react to: the `pdfium-render` crate, the pinned PDFium native build,
/// this adapter, the quality gate, and coordinate reconstruction.
fn extractor_version() -> String {
    format!(
        "pdfium-render=0.9;pdfium-bin={PDFIUM_BUILD};\
         pdf-adapter={PDF_ADAPTER_VERSION};quality={};coords={COORDS_VERSION}",
        quality::QUALITY_VERSION,
    )
}

// --- TOC: the PDF outline ------------------------------------------------

/// Guards against a pathologically deep or cyclic outline graph.
const MAX_TOC_DEPTH: u8 = 30;
const MAX_TOC_ENTRIES: usize = 50_000;

/// Build the [`Toc`] from the PDF outline (`/Outline`), anchoring each
/// entry to a block. An outline entry points at a *target page*, not at
/// a text fragment, so it is anchored to the first block on (or after)
/// that page.
fn build_toc(document: &PdfDocument, blocks: &[Block]) -> Toc {
    let mut raw: Vec<(String, u8, Option<usize>)> = Vec::new();
    if let Some(root) = document.bookmarks().root() {
        walk_bookmarks(root, 0, &mut raw);
    }

    let entries = raw
        .into_iter()
        .map(|(label, depth, page)| TocEntry {
            label,
            depth,
            start_block: page.and_then(|p| anchor_block(blocks, p)),
        })
        .collect();
    Toc { entries }
}

/// Depth-first prefix walk of the outline tree. Siblings are walked
/// iteratively (a flat outline can hold thousands of them); only
/// descent recurses, so recursion depth is bounded by tree depth.
fn walk_bookmarks(first: PdfBookmark, depth: u8, out: &mut Vec<(String, u8, Option<usize>)>) {
    if depth > MAX_TOC_DEPTH {
        return;
    }
    let mut node = Some(first);
    while let Some(current) = node {
        if out.len() >= MAX_TOC_ENTRIES {
            return;
        }
        out.push((
            current.title().unwrap_or_default(),
            depth,
            bookmark_target_page(&current),
        ));
        if let Some(child) = current.first_child() {
            walk_bookmarks(child, depth + 1, out);
        }
        node = current.next_sibling();
    }
}

/// Resolve the 0-based target page of an outline entry. Pdfium exposes
/// the page either through a direct destination or, for a `GoTo`
/// action, through the action's destination — both are tried.
fn bookmark_target_page(node: &PdfBookmark) -> Option<usize> {
    if let Some(dest) = node.destination()
        && let Ok(index) = dest.page_index()
    {
        return Some(index as usize);
    }
    if let Some(action) = node.action()
        && let Some(local) = action.as_local_destination_action()
        && let Ok(dest) = local.destination()
        && let Ok(index) = dest.page_index()
    {
        return Some(index as usize);
    }
    None
}

/// The block an outline entry resolves to: the first block whose source
/// page is the target page or later. Anchoring forward (rather than
/// requiring an exact page) keeps the entry resolvable when its target
/// page carries no extracted block — e.g. a part-title or blank page.
fn anchor_block(blocks: &[Block], target_page: usize) -> Option<usize> {
    blocks
        .iter()
        .position(|b| b.source_unit as usize >= target_page)
}

// --- biblio: the /Info dictionary ----------------------------------------

/// Read every populated `/Info` tag, verbatim and trimmed.
fn read_info_tags(document: &PdfDocument) -> Vec<(&'static str, String)> {
    use PdfDocumentMetadataTagType as Tag;
    let metadata = document.metadata();
    let mut tags = Vec::new();
    for (name, tag) in [
        ("Title", Tag::Title),
        ("Author", Tag::Author),
        ("Subject", Tag::Subject),
        ("Keywords", Tag::Keywords),
        ("Creator", Tag::Creator),
        ("Producer", Tag::Producer),
        ("CreationDate", Tag::CreationDate),
        ("ModificationDate", Tag::ModificationDate),
    ] {
        if let Some(found) = metadata.get(tag) {
            let value = found.value().trim().to_string();
            if !value.is_empty() {
                tags.push((name, value));
            }
        }
    }
    tags
}

/// Map the `/Info` tags onto the `Biblio` contract. A PDF's `/Info`
/// only ever carries title, author, and dates: publisher, ISBN,
/// series, and language have no `/Info` field and stay absent. The
/// author string is transcribed as a single contributor — `/Info`
/// gives no structure to split it on reliably.
///
/// `/Info` is transcribed faithfully, garbage and all: reconciling it
/// against the page text is the METADATA stage's job, not extract's.
fn build_biblio(info_tags: &[(&'static str, String)]) -> Biblio {
    let find = |key: &str| {
        info_tags
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
    };

    let mut contributors = Vec::new();
    if let Some(author) = find("Author") {
        contributors.push(Contributor {
            name: author,
            role: ContributorRole::Author,
        });
    }

    Biblio {
        title: find("Title"),
        year: find("CreationDate").and_then(|d| parse_pdf_year(&d)),
        contributors,
        ..Biblio::default()
    }
}

/// Extract the year from a PDF date string. PDF dates are formatted
/// `D:YYYYMMDDHHmmSSOHH'mm'`; only the leading `YYYY` is needed.
fn parse_pdf_year(date: &str) -> Option<i32> {
    let digits: String = date.trim_start_matches("D:").chars().take(4).collect();
    if digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_digit()) {
        let year: i32 = digits.parse().ok()?;
        if (1000..=9999).contains(&year) {
            return Some(year);
        }
    }
    None
}

/// Borrow the process-wide PDFium handle, loading the native library on
/// first use.
///
/// A load failure is an environment / deployment problem — the pinned
/// binary is missing or unreadable — not a property of any one book.
/// It is reported as [`ExtractError::Io`] with a message naming the
/// directory that was searched: `Io` already means "the host
/// environment could not satisfy this request", so no dedicated
/// contract variant is minted for it.
fn pdfium() -> Result<&'static Pdfium, ExtractError> {
    match PDFIUM.get_or_init(load_pdfium) {
        Ok(pdfium) => Ok(pdfium),
        Err(message) => Err(ExtractError::Io(std::io::Error::other(message.clone()))),
    }
}

/// Bind the PDFium native library from the configured directory. The
/// error is a plain `String` so it can be stored in the `OnceLock` and
/// re-reported on every later call — `PdfiumError` is not `Clone`.
fn load_pdfium() -> Result<Pdfium, String> {
    let dir = bookrack_config::pdfium_lib_dir();
    let library = Pdfium::pdfium_platform_library_name_at_path(&dir);
    Pdfium::bind_to_library(&library)
        .map(Pdfium::new)
        .map_err(|e| {
            format!(
                "PDFium library could not be loaded from {}: {e}",
                dir.display()
            )
        })
}

// --- block assembly ------------------------------------------------------

/// One page's reconstructed paragraphs, tagged with its page number.
struct PageParagraphs {
    page: u32,
    paragraphs: Vec<String>,
}

/// The longest a paragraph can be and still be taken for a running
/// header / footer. Body paragraphs are far longer, so the cap keeps a
/// genuine paragraph that happens to recur from being mistaken for one.
const RUNNING_ELEMENT_MAX_CHARS: usize = 80;

/// The longest a paragraph can be and still be taken for a bare page
/// number.
const PAGE_NUMBER_MAX_CHARS: usize = 6;

/// Flatten per-page paragraphs into ordered body blocks, dropping the
/// running headers, footers, and page numbers that pollute the text.
///
/// Coordinate reconstruction isolates these as their own short
/// paragraphs but cannot tell they are not body text; that judgement
/// needs the whole document. A running header or footer is a short
/// paragraph repeated verbatim across pages; a page number is a short
/// run of digits.
fn build_blocks(pages: Vec<PageParagraphs>) -> Vec<Block> {
    // Which pages each short paragraph appears on. A short paragraph
    // present on two or more pages is a running header or footer.
    let mut pages_of: HashMap<&str, HashSet<u32>> = HashMap::new();
    for page in &pages {
        for paragraph in &page.paragraphs {
            if paragraph.chars().count() <= RUNNING_ELEMENT_MAX_CHARS {
                pages_of
                    .entry(paragraph.as_str())
                    .or_default()
                    .insert(page.page);
            }
        }
    }
    let is_running = |text: &str| pages_of.get(text).is_some_and(|p| p.len() >= 2);

    let mut blocks = Vec::new();
    for page in &pages {
        for paragraph in &page.paragraphs {
            if is_page_number(paragraph) || is_running(paragraph) {
                continue;
            }
            blocks.push(Block {
                kind: BlockKind::Body,
                text: paragraph.clone(),
                source_unit: page.page,
            });
        }
    }
    blocks
}

/// Whether a paragraph is a bare page number — a short run of digits
/// that coordinate reconstruction isolated as its own line.
fn is_page_number(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && trimmed.len() <= PAGE_NUMBER_MAX_CHARS
        && trimmed.bytes().all(|b| b.is_ascii_digit())
}

// --- paragraph reconstruction from glyph coordinates ---------------------

/// One pdfium text segment's geometry: a run of characters sharing a
/// baseline and font, positioned in page coordinates (origin
/// bottom-left, y increasing upward). The segment text is deliberately
/// not kept — see [`build_line`].
struct Seg {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
}

impl Seg {
    fn cy(&self) -> f32 {
        (self.top + self.bottom) / 2.0
    }

    fn height(&self) -> f32 {
        (self.top - self.bottom).abs()
    }
}

/// One reconstructed line of text, with the page-coordinate extent the
/// paragraph splitter needs.
struct Line {
    text: String,
    left: f32,
    cy: f32,
}

/// Reconstruct a page's paragraphs from text geometry: group segments
/// into rows, detect columns, then split each column's lines into
/// paragraphs by spacing and indentation.
fn reconstruct_by_coords(text: &PdfPageText, page_width: f32) -> Vec<String> {
    let mut segments: Vec<Seg> = Vec::new();
    for segment in text.segments().iter() {
        if segment.text().trim().is_empty() {
            continue;
        }
        let bounds = segment.bounds();
        segments.push(Seg {
            left: bounds.left().value,
            right: bounds.right().value,
            top: bounds.top().value,
            bottom: bounds.bottom().value,
        });
    }
    if segments.is_empty() {
        return Vec::new();
    }

    // A reference glyph height drives every later tolerance.
    let heights: Vec<f32> = segments.iter().map(Seg::height).collect();
    let unit = {
        let m = median(&heights);
        if m > 0.1 { m } else { 10.0 }
    };

    // Rows first, then columns: a row groups every segment on one
    // baseline across the full page width, so column detection can ask
    // the robust question "do whole rows cross this x?" rather than the
    // segment-granularity-dependent "do segments cross this x?".
    let rows = group_rows(&segments, unit);
    let columns = detect_columns(&rows, page_width);

    // On a two-column page, a row whose text spans the gutter is a
    // full-width element — a masthead title, a spanning abstract — not
    // column body. It reads as its own band before either column; left
    // in the column pass it would be sliced at the gutter and its halves
    // scattered down the two columns.
    let gutter = (columns.len() == 2).then(|| columns[1].0);

    let mut paragraphs = Vec::new();
    if let Some(gutter) = gutter {
        let band: Vec<Line> = rows
            .iter()
            .filter(|row| spans_gutter(row, gutter))
            .map(|row| build_line(row, text))
            .filter(|line| !line.text.is_empty())
            .collect();
        paragraphs.extend(lines_to_paragraphs(&band, unit));
    }

    for (low, high) in &columns {
        let mut lines: Vec<Line> = Vec::new();
        for row in &rows {
            // A full-width row belongs to the band emitted above, never
            // to a column.
            if gutter.is_some_and(|g| spans_gutter(row, g)) {
                continue;
            }
            let line_segs: Vec<&Seg> = row
                .iter()
                .copied()
                .filter(|s| {
                    let centre = (s.left + s.right) / 2.0;
                    centre >= *low && centre <= *high
                })
                .collect();
            if line_segs.is_empty() {
                continue;
            }
            let line = build_line(&line_segs, text);
            if !line.text.is_empty() {
                lines.push(line);
            }
        }
        paragraphs.extend(lines_to_paragraphs(&lines, unit));
    }
    paragraphs
}

/// Whether a row's text crosses the column gutter — the mark of a
/// full-width element on an otherwise two-column page.
fn spans_gutter(row: &[&Seg], gutter: f32) -> bool {
    row.iter().any(|s| s.left < gutter && s.right > gutter)
}

/// Detect the column layout by scanning for a vertical gutter: the x
/// position crossed by the fewest *rows*. A genuine two-column page has
/// a band no row crosses; on a single-column page every body row runs
/// the full width, so every interior x is crossed. Counting rows, not
/// segments, makes this robust to how finely pdfium happens to split a
/// line into segments.
fn detect_columns(rows: &[Vec<&Seg>], page_width: f32) -> Vec<(f32, f32)> {
    let mut content_left = f32::MAX;
    let mut content_right = f32::MIN;
    for row in rows {
        for s in row {
            content_left = content_left.min(s.left);
            content_right = content_right.max(s.right);
        }
    }
    let whole = vec![(content_left, content_right)];

    let count = rows.len();
    if count < 6 || page_width <= 0.0 {
        return whole;
    }

    let (low, high) = (page_width * 0.30, page_width * 0.70);
    let steps = 40;
    let mut min_crossing = usize::MAX;
    let mut best_x: Vec<f32> = Vec::new();
    for step in 0..=steps {
        let x = low + (high - low) * (step as f32 / steps as f32);
        // A row crosses x if any of its segments spans x.
        let crossing = rows
            .iter()
            .filter(|row| row.iter().any(|s| s.left < x && s.right > x))
            .count();
        let left = rows
            .iter()
            .filter(|row| row.iter().any(|s| s.right <= x))
            .count();
        let right = rows
            .iter()
            .filter(|row| row.iter().any(|s| s.left >= x))
            .count();
        // Both sides must carry a real share of the page's rows.
        if left * 3 < count || right * 3 < count {
            continue;
        }
        if crossing < min_crossing {
            min_crossing = crossing;
            best_x = vec![x];
        } else if crossing == min_crossing {
            best_x.push(x);
        }
    }

    // Two columns need under ~17% of rows crossing the candidate x.
    if min_crossing == usize::MAX || min_crossing * 6 >= count {
        return whole;
    }
    let x = best_x[best_x.len() / 2];

    // Confirm the gutter is a genuine empty stripe, not a per-line seam
    // that pdfium happens to place at a consistent x (observed on some
    // Calibre-produced PDFs, where a line is emitted as two abutting
    // runs). A real column gutter is wide whitespace; a seam is not.
    let mut gaps: Vec<f32> = Vec::new();
    for row in rows {
        let left_edge = row
            .iter()
            .filter(|s| s.right <= x)
            .map(|s| s.right)
            .fold(f32::MIN, f32::max);
        let right_edge = row
            .iter()
            .filter(|s| s.left >= x)
            .map(|s| s.left)
            .fold(f32::MAX, f32::min);
        if left_edge > f32::MIN && right_edge < f32::MAX {
            gaps.push(right_edge - left_edge);
        }
    }
    if !gaps.is_empty() && median(&gaps) > page_width * 0.03 {
        vec![(content_left, x), (x, content_right)]
    } else {
        whole
    }
}

/// Group segments into rows by vertical position. Segments within one
/// row are returned together; rows are ordered top to bottom.
fn group_rows(segments: &[Seg], unit: f32) -> Vec<Vec<&Seg>> {
    let mut order: Vec<usize> = (0..segments.len()).collect();
    // Top to bottom (descending y), with stable tie-breaks.
    order.sort_by(|&a, &b| {
        segments[b]
            .cy()
            .total_cmp(&segments[a].cy())
            .then(segments[a].left.total_cmp(&segments[b].left))
            .then(a.cmp(&b))
    });

    let mut rows: Vec<Vec<&Seg>> = Vec::new();
    let mut row_top = f32::NAN;
    for &i in &order {
        let segment = &segments[i];
        // A segment joins the current row while it stays within one
        // glyph height of that row's topmost segment.
        if rows.is_empty() || (row_top - segment.cy()) > 0.7 * unit {
            rows.push(vec![segment]);
            row_top = segment.cy();
        } else {
            rows.last_mut().unwrap().push(segment);
        }
    }
    rows
}

/// Build one line's text from the union of its segment rectangles.
///
/// The text is read with a single bounded-text query over the whole
/// line, not by concatenating per-segment text: pdfium's per-segment
/// bounded-text query drops the spaces that sit on segment-rectangle
/// edges, so a join would weld adjacent words together.
fn build_line(segments: &[&Seg], text: &PdfPageText) -> Line {
    let left = segments.iter().map(|s| s.left).fold(f32::MAX, f32::min);
    let right = segments.iter().map(|s| s.right).fold(f32::MIN, f32::max);
    let top = segments.iter().map(|s| s.top).fold(f32::MIN, f32::max);
    let bottom = segments.iter().map(|s| s.bottom).fold(f32::MAX, f32::min);
    let cy = segments.iter().map(|s| s.cy()).sum::<f32>() / segments.len() as f32;

    // A small horizontal margin keeps the first and last glyphs from
    // falling outside the query box; the column gutter is far wider, so
    // this cannot reach into a neighbouring column.
    let rect = PdfRect::new_from_values(bottom, left - 2.0, top, right + 2.0);
    let raw = text.inside_rect(rect);
    // Collapse the line's internal whitespace (and any stray newline)
    // to single spaces; this leaves CJK, which has none, untouched.
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");

    Line {
        text: normalized,
        left,
        cy,
    }
}

/// Split a column's ordered lines into paragraphs. A paragraph break is
/// taken where the inter-line gap is markedly larger than the column's
/// usual line spacing, or where a line begins with a first-line
/// indent — the two ways books mark a new paragraph.
fn lines_to_paragraphs(lines: &[Line], unit: f32) -> Vec<String> {
    let lines: Vec<&Line> = lines.iter().filter(|l| !l.text.is_empty()).collect();
    if lines.is_empty() {
        return Vec::new();
    }

    // The column's body-text left edge is the *typical* line start, not
    // the minimum: a page number, running header, or figure caption can
    // sit further left than the body, and taking the min would then make
    // every body line look first-line-indented.
    let lefts: Vec<f32> = lines.iter().map(|l| l.left).collect();
    let column_left = median(&lefts);
    let gaps: Vec<f32> = lines
        .windows(2)
        .map(|w| (w[0].cy - w[1].cy).max(0.0))
        .collect();
    let normal_gap = if gaps.is_empty() { unit } else { median(&gaps) };
    let indent = 0.8 * unit;

    let mut paragraphs = Vec::new();
    let mut current = String::new();
    for (i, line) in lines.iter().enumerate() {
        let starts_paragraph =
            i == 0 || gaps[i - 1] > normal_gap * 1.5 || line.left > column_left + indent;
        if starts_paragraph {
            push_paragraph(&mut paragraphs, &mut current);
            current.push_str(&line.text);
        } else {
            append_line(&mut current, &line.text);
        }
    }
    push_paragraph(&mut paragraphs, &mut current);
    paragraphs
}

/// The median of a slice of measurements. The slice is small (segment
/// heights or line gaps on one page), so a copy-and-sort is fine.
fn median(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    sorted[sorted.len() / 2]
}

/// Flush the paragraph being built into `out`, if it holds any text.
fn push_paragraph(out: &mut Vec<String>, current: &mut String) {
    let text = current.trim();
    if !text.is_empty() {
        out.push(text.to_string());
    }
    current.clear();
}

/// Append a soft-wrapped line to the paragraph being built — without a
/// space between two CJK characters, with a space otherwise, and
/// de-hyphenating a Latin word broken across the line break.
fn append_line(current: &mut String, line: &str) {
    if current.is_empty() {
        current.push_str(line);
        return;
    }
    let prev = current.chars().last().unwrap_or(' ');
    let next = line.chars().next().unwrap_or(' ');
    if (prev == '-' || prev == '\u{00AD}') && next.is_ascii_lowercase() {
        // A Latin word hyphenated across the break — drop the hyphen.
        current.pop();
        current.push_str(line);
    } else if quality::is_cjk(prev) && quality::is_cjk(next) {
        // A CJK line break carries no space.
        current.push_str(line);
    } else {
        current.push(' ');
        current.push_str(line);
    }
}
