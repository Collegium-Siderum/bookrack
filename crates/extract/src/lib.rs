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
mod epub;
mod html;
mod html_parse;
mod txt;

pub use contract::*;

use std::path::Path;

/// Extract one source file into an [`ExtractOutcome`].
///
/// The format is detected from the file extension. The outcome is
/// three-state: a usable text layer yields [`ExtractOutcome::Extracted`]
/// with the format-neutral [`Extraction`]; a file with a text layer too
/// poor to use — only PDF can produce this today — yields
/// [`ExtractOutcome::NeedsOcr`] to route the file onto the OCR path; and
/// a structural failure (an unreadable file, an unsupported format) is
/// an [`ExtractError`].
pub fn extract(path: &Path) -> Result<ExtractOutcome, ExtractError> {
    match detect::detect(path) {
        detect::Format::Epub => epub::extract(path).map(ExtractOutcome::Extracted),
        detect::Format::Html => html::extract(path).map(ExtractOutcome::Extracted),
        detect::Format::Txt => txt::extract(path).map(ExtractOutcome::Extracted),
        other => Err(ExtractError::UnsupportedFormat {
            detected: other.label().to_string(),
        }),
    }
}
