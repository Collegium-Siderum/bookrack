// SPDX-License-Identifier: Apache-2.0

//! Gate for the tests that need the PDFium native library.
//!
//! The binary is not vendored, so a contributor without it still gets a
//! green `cargo test`: the tests that need PDFium return early. A test
//! harness cannot tell that early return from a real pass, so an
//! environment that was meant to carry the binary loses the coverage
//! without saying so.
//!
//! [`available`] closes both halves of that. It probes the PDF
//! adapter's own loader — the same search chain and the same failure —
//! so a caller in any crate sees exactly what an extraction would see,
//! and where the environment declares PDFium mandatory it turns the
//! absent case into a panic instead of a skip.
//!
//! The two-state handling itself is [`crate::gate::Gate`]; PDFium is
//! its first user, and the wording below is what that mechanism was
//! generalised out of.

use std::error::Error;

use crate::gate::Gate;
use crate::{ExtractError, pdf};

/// Environment variable declaring whether the PDFium binary is
/// mandatory here. A truthy value turns an absent library into a
/// failure; a falsy one forces the skip even under a CI runner. When
/// unset, a non-empty `CI` decides.
pub const REQUIRE_ENV: &str = "BOOKRACK_REQUIRE_PDFIUM";

const GATE: Gate = Gate::new("PDFium", "PDFium native library", "PDF test", REQUIRE_ENV);

/// Whether the PDFium native library can be loaded in this process.
///
/// Loading is attempted once per process and the outcome cached, so
/// repeated calls across a test binary cost nothing after the first.
///
/// # Panics
///
/// When the library cannot be loaded and the environment declares it
/// mandatory (see [`REQUIRE_ENV`]): there, a skipped PDF test is a
/// silently lost guarantee, not a courtesy to the contributor.
pub fn available() -> bool {
    GATE.check(|| pdf::pdfium().map(|_| ()).map_err(|e| reason(&e)))
}

/// Flatten a load failure into one line. The variant's own display is
/// a category (`ExtractError::Io` prints "I/O error"); the loader's
/// report — every directory searched, and the remedies — is the source
/// underneath it, so both halves are joined here rather than letting
/// the actionable half fall off the message.
fn reason(e: &ExtractError) -> String {
    match e.source() {
        Some(source) => format!("{e}: {source}"),
        None => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_carries_the_loaders_own_report() {
        let report = "PDFium library libpdfium.so not found; searched: /one, /two. \
                      Run `bookrack doctor --install-pdfium`";
        let text = reason(&ExtractError::Io(std::io::Error::other(report)));
        assert!(
            text.contains(report),
            "the directories searched and the remedies must survive into the message, got {text}",
        );
    }
}
