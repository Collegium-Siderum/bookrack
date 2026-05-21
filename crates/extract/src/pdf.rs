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
//! Paragraphs are reconstructed by a blank-line heuristic: within a
//! page, soft-wrapped lines are joined and a blank line ends a
//! paragraph. This is a deliberately simple first cut; a later commit
//! replaces it with reconstruction from glyph coordinates, which can
//! recover multi-column reading order and true paragraph breaks. The
//! PDF outline and `/Info` bibliography are likewise not lifted yet.
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

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use pdfium_render::prelude::*;

use crate::contract::{
    Biblio, Block, BlockKind, ExtractError, ExtractOutcome, Extraction, Provenance, SkippedUnit,
    Toc,
};
use crate::quality::{self, QualityDecision};

/// Behaviour-sensitive version of this adapter. Bump when a change here
/// shifts block boundaries or the extraction outcome.
const PDF_ADAPTER_VERSION: u32 = 1;

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

    // Pull each page's text. A page whose text pdfium cannot read is
    // skipped and recorded — not fatal: the rest of the book is still
    // worth extracting (see `ExtractError`'s contract). `page_numbers`
    // keeps each kept page's index so a block's `source_unit` stays the
    // true page number even when an earlier page was skipped.
    let mut pages_text: Vec<String> = Vec::new();
    let mut page_numbers: Vec<u32> = Vec::new();
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
                pages_text.push(text.all());
                page_numbers.push(index);
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

    let mut blocks: Vec<Block> = Vec::new();
    for (text, &page) in pages_text.iter().zip(&page_numbers) {
        for paragraph in split_paragraphs(text) {
            blocks.push(Block {
                kind: BlockKind::Body,
                text: paragraph,
                source_unit: page,
            });
        }
    }

    Ok(ExtractOutcome::Extracted(Extraction {
        blocks,
        // The PDF outline and `/Info` dictionary graduate in the next
        // commit; until then a PDF carries no TOC or bibliography.
        toc: Toc::default(),
        biblio: Biblio::default(),
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
/// this adapter, and the quality gate. `para=line-heuristic` records
/// that paragraphs come from the blank-line heuristic — coordinate
/// reconstruction replaces that marker when it lands.
fn extractor_version() -> String {
    format!(
        "pdfium-render=0.9;pdfium-bin={PDFIUM_BUILD};\
         pdf-adapter={PDF_ADAPTER_VERSION};quality={};para=line-heuristic",
        quality::QUALITY_VERSION,
    )
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

// --- paragraph reconstruction: line heuristic ----------------------------

/// Split one page's extracted text into paragraphs. A blank line ends a
/// paragraph; within a paragraph, soft-wrapped lines are joined —
/// without a space between two CJK characters, with a space otherwise,
/// and de-hyphenating a Latin word broken across the line break.
fn split_paragraphs(page: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    for line in page.lines() {
        let line = line.trim();
        if line.is_empty() {
            push_paragraph(&mut paragraphs, &mut current);
        } else {
            append_line(&mut current, line);
        }
    }
    push_paragraph(&mut paragraphs, &mut current);
    paragraphs
}

/// Flush the paragraph being built into `out`, if it holds any text.
fn push_paragraph(out: &mut Vec<String>, current: &mut String) {
    let text = current.trim();
    if !text.is_empty() {
        out.push(text.to_string());
    }
    current.clear();
}

/// Append a soft-wrapped line to the paragraph being built.
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
