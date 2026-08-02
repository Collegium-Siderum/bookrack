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

use bookrack_core::knob::{
    Candidate, DotenvSupply, KnobOrigin, KnobReach, Layer, ReadAt, env_layers, resolve_knob,
};

use crate::gate::Gate;
use crate::{ExtractError, pdf};

/// Environment variable declaring whether the PDFium binary is
/// mandatory here. A truthy value turns an absent library into a
/// failure; a falsy one forces the skip even under a CI runner. When
/// unset, a non-empty `CI` decides.
pub const REQUIRE_ENV: &str = "BOOKRACK_REQUIRE_PDFIUM";

const GATE: Gate = Gate::new("PDFium", "PDFium native library", "PDF test", REQUIRE_ENV);

/// Every knob this module reads, with where the value came from.
///
/// One row, and it is a test-harness knob rather than a runtime one:
/// [`REQUIRE_ENV`] decides whether a missing PDFium fails a test run or
/// skips it, and nothing outside a test consults it. It is reported
/// anyway, and says so in its site, because "why was the PDF test
/// skipped" has no other answer on a CI machine.
pub fn knob_origins(dotenv: Option<DotenvSupply<'_>>) -> Vec<KnobOrigin> {
    knob_origins_from(|name| std::env::var(name).ok(), dotenv)
}

/// The same row on a machine where nothing is configured: the inventory
/// form, so a list of this build's knobs can include the one that
/// decides whether a missing PDFium fails a test run.
pub fn knob_catalog() -> Vec<KnobOrigin> {
    knob_origins_from(|_| None, None)
}

/// Pure form of [`knob_origins`], so the inventory can describe an
/// environment that sets neither the dedicated variable nor `CI`.
fn knob_origins_from(
    get: impl Fn(&str) -> Option<String>,
    dotenv: Option<DotenvSupply<'_>>,
) -> Vec<KnobOrigin> {
    let mut candidates = env_layers(
        dotenv,
        REQUIRE_ENV,
        get(REQUIRE_ENV)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
    );
    candidates.push(Candidate::of(
        Layer::Environment,
        "CI",
        get("CI")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
    ));
    candidates.push(Candidate::of(
        Layer::Default,
        "built-in (test harness only)",
        Some("false".to_string()),
    ));

    vec![resolve_knob(
        "pdfium.required",
        KnobReach::Process,
        ReadAt::PerCall,
        candidates,
    )]
}

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

    /// The row says in its own site that it governs a test harness, so
    /// a reader of the effective-configuration table is not left to
    /// infer that this knob changes what bookrack does at runtime.
    #[test]
    fn the_row_declares_itself_a_test_harness_knob() {
        let rows = knob_origins(None);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        assert_eq!(row.key, "pdfium.required");
        let mentions_harness = row.site.contains("test harness")
            || row.shadowed.iter().any(|s| s.site.contains("test harness"));
        assert!(
            mentions_harness,
            "no layer of the row says it is a test-harness knob: site={:?} shadowed={:?}",
            row.site, row.shadowed
        );
    }

    /// The catalog row falls to the built-in default and names both
    /// variables as places it can be moved from.
    ///
    /// Fully discriminating under CI, which sets `CI` non-blank: a
    /// catalog wired to [`knob_origins`] instead of the pure form would
    /// report that site as the winner there, so the test that passes on
    /// a developer machine either way still fails in the environment
    /// that matters.
    #[test]
    fn the_catalog_falls_to_the_built_in_default() {
        let rows = knob_catalog();
        let row = &rows[0];

        assert_eq!(row.layer, Layer::Default, "site: {}", row.site);
        assert_eq!(row.value.as_deref(), Some("false"));
        let sites: Vec<&str> = row.chain.iter().map(|s| s.site.as_str()).collect();
        assert!(sites.contains(&REQUIRE_ENV), "{sites:?}");
        assert!(sites.contains(&"CI"), "{sites:?}");
    }

    /// `CI` is a second environment input alongside the dedicated
    /// variable, and the row keeps them as separate sites.
    #[test]
    fn ci_is_a_site_of_its_own_beside_the_require_variable() {
        let rows = knob_origins(None);
        let row = &rows[0];
        let sites: Vec<&str> = std::iter::once(row.site.as_str())
            .chain(row.shadowed.iter().map(|s| s.site.as_str()))
            .collect();
        assert!(
            sites
                .iter()
                .any(|s| *s == REQUIRE_ENV || *s == "CI" || s.contains("built-in")),
            "row names none of its own layers: {sites:?}"
        );
    }
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
