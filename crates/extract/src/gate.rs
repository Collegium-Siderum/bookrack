// SPDX-License-Identifier: Apache-2.0

//! Two-state handling for a host resource a test needs.
//!
//! A test that returns early when its resource is missing is
//! indistinguishable, to the harness, from a test that ran and passed.
//! That is the right trade on a contributor's machine, which may
//! legitimately not carry a native library — and the wrong one
//! everywhere the resource was meant to be present, because the
//! coverage is lost without anything saying so.
//!
//! A [`Gate`] holds both states at once. Where the resource is
//! optional, an absence is explained on stderr and the test returns
//! early. Where the environment declares it mandatory, an absence is a
//! panic. Which of the two applies is that resource's own `REQUIRE`
//! variable, falling back to `CI` — the variable every provider sets —
//! when it says nothing.

/// One host resource a test needs, and what its absence means.
///
/// The three names are separate because they appear in different
/// sentences, and both sentences are read by someone trying to work out
/// what went wrong.
pub struct Gate {
    /// The resource as the mandatory-environment panic names it, e.g.
    /// `"PDFium"`.
    resource: &'static str,
    /// The noun phrase the skip line reports as unavailable, e.g.
    /// `"PDFium native library"`.
    subject: &'static str,
    /// What is being skipped, e.g. `"PDF test"`.
    skipped: &'static str,
    /// Variable declaring whether this resource is mandatory here.
    require_env: &'static str,
}

impl Gate {
    /// Declare a gate. `const` so a consumer can hold one in a `static`.
    pub const fn new(
        resource: &'static str,
        subject: &'static str,
        skipped: &'static str,
        require_env: &'static str,
    ) -> Gate {
        Gate {
            resource,
            subject,
            skipped,
            require_env,
        }
    }

    /// Run `probe` and report whether the resource is usable.
    ///
    /// `probe` returns `Err(reason)` when the resource is unavailable;
    /// the reason is carried into whichever of the two outcomes
    /// applies, so the caller's own diagnosis — the directories it
    /// searched, the remedy it suggests — reaches the reader either
    /// way.
    ///
    /// # Panics
    ///
    /// When the resource is unavailable and the environment declares it
    /// mandatory: a skipped test there is a silently lost guarantee,
    /// not a courtesy to the contributor.
    pub fn check(&self, probe: impl FnOnce() -> Result<(), String>) -> bool {
        match probe() {
            Ok(()) => true,
            Err(reason) => self.unavailable(&reason, self.required()),
        }
    }

    /// Resolve the absent case: a note on stderr and `false` where the
    /// resource is optional, a panic where it is mandatory.
    fn unavailable(&self, reason: &str, required: bool) -> bool {
        assert!(
            !required,
            "{} is unavailable ({reason}), and {} / CI declares it mandatory here, \
             so the tests that need it must not be skipped",
            self.resource, self.require_env,
        );
        eprintln!(
            "skipping {}: {} unavailable ({reason})",
            self.skipped, self.subject,
        );
        false
    }

    /// Whether the environment declares this resource mandatory.
    fn required(&self) -> bool {
        required_from(
            std::env::var(self.require_env).ok(),
            std::env::var("CI").ok(),
        )
    }
}

/// Pure policy behind [`Gate::required`], factored out so both branches
/// can be tested without mutating process-global environment variables.
///
/// `require` is authoritative when it carries a non-blank value, so a
/// runner that genuinely cannot supply the resource can opt out of the
/// requirement; otherwise a non-blank `ci` — the variable every CI
/// provider sets — makes it mandatory.
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

    const TEST_GATE: Gate = Gate::new(
        "Widgetron",
        "Widgetron runtime",
        "widget test",
        "BOOKRACK_REQUIRE_WIDGETRON",
    );

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

    /// The optional half: the caller is told, and the test returns
    /// early. An implementation that stayed silent would look identical
    /// to a pass.
    #[test]
    fn an_optional_environment_skips_and_says_so() {
        assert!(!TEST_GATE.unavailable("nothing found", false));
    }

    /// The mandatory half, and the reason both names are carried: the
    /// panic has to say which resource and which variable, or the
    /// reader has neither the diagnosis nor the opt-out.
    #[test]
    #[should_panic(expected = "Widgetron is unavailable (nothing found)")]
    fn a_mandatory_environment_panics_instead_of_skipping() {
        TEST_GATE.unavailable("nothing found", true);
    }

    #[test]
    #[should_panic(expected = "BOOKRACK_REQUIRE_WIDGETRON / CI declares it mandatory")]
    fn the_panic_names_the_variable_that_opts_out() {
        TEST_GATE.unavailable("nothing found", true);
    }

    /// A present resource is unaffected by the require flag: the flag
    /// governs what an absence means, not whether the probe runs.
    #[test]
    fn a_present_resource_is_not_affected_by_the_require_flag() {
        assert!(TEST_GATE.check(|| Ok(())));
    }
}
