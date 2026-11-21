// SPDX-License-Identifier: Apache-2.0

//! Gate for the tests that need the PDFium native library.
//!
//! The binary is not vendored, so a contributor without it still gets a
//! green `cargo test`: the tests that need PDFium return early. A test
//! harness cannot tell that early return from a real pass, so an
//! environment that was meant to carry the binary loses the coverage
//! without saying so.
//!
//! [`available`] closes both halves of that. It reads the PDF
//! adapter's own load outcome — the same search chain and the same
//! failure — so a caller in any crate sees exactly what an extraction
//! would see, and under `CI` — where PDFium is mandatory — it turns
//! the absent case into a panic instead of a skip. A library that
//! is found and will not bind — another build, or a file that is not a
//! loadable library at all — is a panic in every environment: that is a
//! misconfigured machine, not one without PDFium.
//!
//! The outcomes themselves are [`crate::gate::Gate`]; PDFium is its
//! first user, and the wording below is what that mechanism was
//! generalised out of.

use crate::gate::{Gate, Unavailable};
use crate::pdf::{self, LoadFailure};
use crate::pdfium_pin::PDFIUM_RELEASE_TAG;

const GATE: Gate = Gate::new("PDFium", "PDFium native library", "PDF test");

/// Whether the PDFium native library can be loaded in this process.
///
/// Loading is attempted once per process and the outcome cached, so
/// repeated calls across a test binary cost nothing after the first.
///
/// # Panics
///
/// When no library is found and `CI` is set: there, a skipped PDF test
/// is a silently lost guarantee, not a courtesy to the contributor.
/// When a library is found and will not bind, in every environment.
pub fn available() -> bool {
    GATE.check(|| pdf::pdfium_load().map_err(classify))
}

/// Sort a load failure into absent or broken by whether the loader
/// found a directory to bind from. The loader's own report — every
/// directory searched and the remedy, or the directory and the
/// binding error — is carried verbatim; a broken library adds the
/// build this binary binds and the way to put that build in its place.
fn classify(failure: &LoadFailure) -> Unavailable {
    if !failure.found {
        return Unavailable::Absent(failure.message.clone());
    }
    Unavailable::Broken(format!(
        "{message}; this build binds PDFium `{PDFIUM_RELEASE_TAG}`. \
         Remove the library that was found, then run `bookrack setup pdfium`, \
         which installs that build",
        message = failure.message,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_library_that_is_found_but_will_not_bind_is_broken() {
        let report = "PDFium library could not be loaded from /lib/other: \
                      symbol FPDF_ExampleCall not found";
        let outcome = classify(&LoadFailure {
            found: true,
            message: report.to_string(),
        });
        let Unavailable::Broken(text) = outcome else {
            panic!("a library that was found and failed to bind must be broken, got {outcome:?}");
        };
        assert!(text.contains(report), "the loader's report is lost: {text}");
        assert!(
            text.contains(PDFIUM_RELEASE_TAG),
            "the build this binary binds is not named: {text}"
        );
        assert!(
            text.contains("bookrack setup pdfium"),
            "the command that installs the pinned build is not named: {text}"
        );
    }

    #[test]
    fn a_library_that_is_not_found_is_absent() {
        let report = "PDFium library libpdfium.so not found; searched: /one, /two. \
                      Run `bookrack setup pdfium`";
        let outcome = classify(&LoadFailure {
            found: false,
            message: report.to_string(),
        });
        assert_eq!(outcome, Unavailable::Absent(report.to_string()));
    }
}
