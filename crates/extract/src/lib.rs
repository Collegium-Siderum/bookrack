// SPDX-License-Identifier: Apache-2.0

//! extract: format adapters that turn a source file into a
//! format-neutral [`Extraction`] — ordered content blocks, a TOC tree,
//! and bibliographic metadata. The deliverable feeds STRUCTURE and
//! METADATA downstream.
//!
//! An adapter is a pure, synchronous function of the source file: same
//! file plus same extractor versions yields a byte-identical
//! `Extraction`. The crate does no chunking, no normalization, no tree
//! building, and no database writes — those belong to later stages.

mod contract;
mod detect;
pub mod envelope;
mod epub;
mod headings;
mod html;
mod html_parse;
pub mod ocr;
mod pdf;
mod pdf_paper;
pub mod pdfium_pin;
mod quality;
mod txt;

pub use contract::*;
pub use envelope::{
    ENVELOPE_FILE_SUFFIX, ENVELOPE_SCHEMA_VERSION, EnvelopeError, ExtractionEnvelope,
    envelope_filename, envelope_filename_kinded, envelope_filename_legacy, read_envelope,
    read_envelope_with_fallback, write_envelope,
};
pub use pdf_paper::{extract_paper_abstract, reason as paper_abstract_reason};

use std::path::Path;

use bookrack_audit_profile::{AuditProfile, HeadingPatterns};

/// Monotonic version of the extractor's output. Stored on
/// `intake.extractor_version`; a mismatch with a stored row marks the
/// partition stale.
///
/// Bumped whenever the shape or interpretation of [`Extraction`]
/// changes, or whenever a behaviour-sensitive dependency is upgraded.
/// The companion test `tests/dep_hash.rs` fails until
/// [`FROZEN_DEPS_HASH`] is refreshed, forcing a deliberate bump.
pub const EXTRACTOR_VERSION: u32 = 5;

/// Monotonic version of the OCR adapter's output. Stored on the OCR
/// intake's `intake.extractor_version`, decoupled from
/// [`EXTRACTOR_VERSION`] so a bump on one side does not mark partitions
/// on the other side stale. The catalog's
/// `stale_partitions` / `stale_ocr_partitions` queries make that split
/// load-bearing.
///
/// Bumped whenever the OCR adapter's parsing rules change in a way that
/// could shift block boundaries: the marker grammar, the frontmatter
/// handling, or the paragraph segmentation.
pub const OCR_INTAKE_VERSION: u32 = 1;

/// SHA-256 of the sorted `name@version` lines of the behaviour-
/// sensitive crates the extractor depends on. Refreshed in lockstep
/// with [`EXTRACTOR_VERSION`]. See `tests/dep_hash.rs` for the input
/// list and computation.
pub const FROZEN_DEPS_HASH: &str =
    "b4ad0eff4a4f766e81081f72e9313a2ef7ffb0dd60e8bcfb8e05e1aaf4d806ae";

/// Extract one source file into an [`ExtractOutcome`].
///
/// The format is detected from the file extension. The outcome is
/// three-state: a usable text layer yields [`ExtractOutcome::Extracted`]
/// with the format-neutral [`Extraction`]; a file with a text layer too
/// poor to use — only PDF can produce this today — yields
/// [`ExtractOutcome::NeedsOcr`] to route the file onto the OCR path; and
/// a structural failure (an unreadable file, an unsupported format) is
/// an [`ExtractError`].
///
/// `profile` carries every behavioural knob the adapters consult:
/// `profile.extract` gates the EPUB / TXT half-rules,
/// `profile.html` carries the block / skip tag lists the EPUB and
/// HTML adapters' DOM walk uses, and `profile.quality` carries the
/// thresholds the PDF text-layer gate consults. `heading_patterns`
/// carries the multi-language chapter / volume marker grammar the
/// TXT adapter consults.
pub fn extract(
    path: &Path,
    profile: &AuditProfile,
    heading_patterns: &HeadingPatterns,
) -> Result<ExtractOutcome, ExtractError> {
    match detect::detect(path) {
        detect::Format::Epub => {
            epub::extract(path, &profile.extract, &profile.html).map(ExtractOutcome::Extracted)
        }
        detect::Format::Html => html::extract(path, &profile.html).map(ExtractOutcome::Extracted),
        detect::Format::Txt => {
            txt::extract(path, &profile.extract, heading_patterns).map(ExtractOutcome::Extracted)
        }
        // The PDF adapter resolves the three-state outcome itself — a
        // PDF can route to OCR — so it is not wrapped like the others.
        detect::Format::Pdf => pdf::extract(path, &profile.quality),
        other => Err(ExtractError::UnsupportedFormat {
            detected: other.label().to_string(),
        }),
    }
}
