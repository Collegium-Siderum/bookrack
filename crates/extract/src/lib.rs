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
mod pdf;
mod quality;
mod txt;

pub use contract::*;

use std::path::Path;

use bookrack_audit_profile::ExtractToggles;

/// Monotonic version of the extractor's output. Stored on
/// `intake.extractor_version`; a mismatch with a stored row marks the
/// partition stale.
///
/// Bumped whenever the shape or interpretation of [`Extraction`]
/// changes, or whenever a behaviour-sensitive dependency is upgraded.
/// The companion test `tests/dep_hash.rs` fails until
/// [`FROZEN_DEPS_HASH`] is refreshed, forcing a deliberate bump.
pub const EXTRACTOR_VERSION: u32 = 1;

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
/// `toggles` carries the four half-rule switches the EPUB and TXT
/// adapters consult: EPUB year-range gating, EPUB ISBN recognition,
/// MARC role-code mapping, and TXT heading detection. Format-detect,
/// HTML, and PDF adapters do not yet consume the toggle bag.
pub fn extract(path: &Path, toggles: &ExtractToggles) -> Result<ExtractOutcome, ExtractError> {
    match detect::detect(path) {
        detect::Format::Epub => epub::extract(path, toggles).map(ExtractOutcome::Extracted),
        detect::Format::Html => html::extract(path).map(ExtractOutcome::Extracted),
        detect::Format::Txt => txt::extract(path, toggles).map(ExtractOutcome::Extracted),
        // The PDF adapter resolves the three-state outcome itself — a
        // PDF can route to OCR — so it is not wrapped like the others.
        detect::Format::Pdf => pdf::extract(path),
        other => Err(ExtractError::UnsupportedFormat {
            detected: other.label().to_string(),
        }),
    }
}
