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

use std::error::Error;

use crate::{ExtractError, pdf};

/// Environment variable declaring whether the PDFium binary is
/// mandatory here. A truthy value turns an absent library into a
/// failure; a falsy one forces the skip even under a CI runner. When
/// unset, a non-empty `CI` decides.
pub const REQUIRE_ENV: &str = "BOOKRACK_REQUIRE_PDFIUM";

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
    match pdf::pdfium() {
        Ok(_) => true,
        Err(e) => unavailable(&reason(&e), required()),
    }
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

/// Resolve the absent-library case: a note on stderr and `false` where
/// PDFium is optional.
///
/// # Panics
///
/// When `required`, with the loader's own report — which names every
/// directory searched and the remedies — as the failure message.
fn unavailable(reason: &str, required: bool) -> bool {
    assert!(
        !required,
        "PDFium is unavailable ({reason}), and {REQUIRE_ENV} / CI declares it mandatory here, \
         so the tests that need it must not be skipped",
    );
    eprintln!("skipping PDF test: PDFium native library unavailable ({reason})");
    false
}

/// Whether the environment declares the PDFium binary mandatory.
fn required() -> bool {
    required_from(std::env::var(REQUIRE_ENV).ok(), std::env::var("CI").ok())
}

/// Pure policy behind [`required`], factored out so both branches can
/// be tested without mutating process-global environment variables.
///
/// `require` is authoritative when it carries a non-blank value, so a
/// runner that genuinely cannot supply the binary can opt out of the
/// requirement; otherwise a non-blank `ci` — the variable every CI
/// provider sets — makes the binary mandatory.
fn required_from(require: Option<String>, ci: Option<String>) -> bool {
    match require {
        Some(v) if !v.trim().is_empty() => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        _ => ci.is_some_and(|v| !v.trim().is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_decides_whenever_it_carries_a_value() {
        assert!(required_from(Some("1".into()), None));
        assert!(required_from(Some("true".into()), None));
        assert!(required_from(Some("yes".into()), None));
        // A falsy override wins over a CI runner.
        assert!(!required_from(Some("0".into()), Some("true".into())));
        assert!(!required_from(Some("False".into()), Some("true".into())));
        assert!(!required_from(Some("off".into()), Some("1".into())));
    }

    #[test]
    fn ci_decides_when_the_override_is_absent_or_blank() {
        assert!(required_from(None, Some("true".into())));
        assert!(required_from(Some("   ".into()), Some("1".into())));
        assert!(!required_from(None, None));
        assert!(!required_from(None, Some("".into())));
        assert!(!required_from(Some(" ".into()), Some("  ".into())));
    }

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

    #[test]
    fn an_absent_library_is_a_skip_where_it_is_optional() {
        assert!(!unavailable("library not found", false));
    }

    #[test]
    #[should_panic(expected = "declares it mandatory here")]
    fn an_absent_library_is_a_failure_where_the_environment_demands_it() {
        unavailable("library not found", true);
    }
}
