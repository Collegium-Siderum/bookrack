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
mod html_parse;

pub use contract::*;

use std::path::Path;

/// Extract one source file into the format-neutral [`Extraction`].
///
/// The format is detected from the file extension. An unrecognized
/// format, or one not yet supported, is reported as
/// [`ExtractError::UnsupportedFormat`] rather than guessed at.
pub fn extract(path: &Path) -> Result<Extraction, ExtractError> {
    match detect::detect(path) {
        detect::Format::Epub => epub::extract(path),
        other => Err(ExtractError::UnsupportedFormat {
            detected: other.label().to_string(),
        }),
    }
}
