// SPDX-License-Identifier: Apache-2.0

//! Three outcomes for a host resource a test needs.
//!
//! A test that returns early when its resource is missing is
//! indistinguishable, to the harness, from a test that ran and passed.
//! That is the right trade on a contributor's machine, which may
//! legitimately not carry a native library — and the wrong one
//! everywhere the resource was meant to be present, because the
//! coverage is lost without anything saying so.
//!
//! A [`Gate`] tells the cases apart. A usable resource runs the test.
//! An absent one depends on the environment: where the resource is
//! optional, the absence is explained on stderr and the test returns
//! early; where the environment declares it mandatory, the absence is a
//! panic. Which of the two applies is `CI`, the variable every
//! provider sets: a runner is where the resource was meant to be
//! present. A resource that is found and cannot be used is a panic in
//! every environment: the skip is for a machine that does not carry the
//! resource, and a machine that carries an unusable one is
//! misconfigured rather than exempt.

/// Why a probed resource cannot be used.
#[derive(Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// Nothing was found where the resource is searched for.
    Absent(String),
    /// Something was found and cannot be used.
    Broken(String),
}

/// One host resource a test needs, and what its absence means.
///
/// The three names are separate because they appear in different
/// sentences, each read by someone trying to work out what went wrong.
pub struct Gate {
    /// The resource as the mandatory-environment panic names it, e.g.
    /// `"PDFium"`.
    resource: &'static str,
    /// The noun phrase the skip line reports as unavailable, e.g.
    /// `"PDFium native library"`.
    subject: &'static str,
    /// What is being skipped, e.g. `"PDF test"`.
    skipped: &'static str,
}

impl Gate {
    /// Declare a gate. `const` so a consumer can hold one in a `static`.
    pub const fn new(resource: &'static str, subject: &'static str, skipped: &'static str) -> Gate {
        Gate {
            resource,
            subject,
            skipped,
        }
    }

    /// Run `probe` and report whether the resource is usable.
    ///
    /// `probe` returns `Err` when the resource is unavailable, saying
    /// whether it is absent or broken; the reason it carries reaches
    /// whichever outcome applies, so the caller's own diagnosis — the
    /// directories it searched, the remedy it suggests — reaches the
    /// reader either way.
    ///
    /// # Panics
    ///
    /// When the resource is absent and the environment declares it
    /// mandatory: a skipped test there is a silently lost guarantee,
    /// not a courtesy to the contributor. When the resource is broken,
    /// whatever the environment declares.
    pub fn check(&self, probe: impl FnOnce() -> Result<(), Unavailable>) -> bool {
        match probe() {
            Ok(()) => true,
            Err(outcome) => self.resolve(outcome, self.required()),
        }
    }

    /// Resolve an unavailable resource, with `required` passed in
    /// rather than read from the environment: an absence goes to
    /// [`Gate::unavailable`], a broken resource to [`Gate::broken`],
    /// which does not consult `required` at all.
    fn resolve(&self, outcome: Unavailable, required: bool) -> bool {
        match outcome {
            Unavailable::Absent(reason) => self.unavailable(&reason, required),
            Unavailable::Broken(reason) => self.broken(&reason),
        }
    }

    /// Resolve the broken case: a panic in every environment, saying
    /// that the skip an unset `CI` allows covers an absence and not
    /// this.
    fn broken(&self, reason: &str) -> ! {
        panic!(
            "{} was found but cannot be used ({reason}); only a {} that is absent is \
             skipped where CI is unset, so this fails whether or not it is set",
            self.resource, self.subject,
        )
    }

    /// Resolve the absent case: a note on stderr and `false` where the
    /// resource is optional, a panic where it is mandatory.
    fn unavailable(&self, reason: &str, required: bool) -> bool {
        assert!(
            !required,
            "{} is unavailable ({reason}), and CI declares it mandatory here, \
             so the tests that need it must not be skipped",
            self.resource,
        );
        eprintln!(
            "skipping {}: {} unavailable ({reason})",
            self.skipped, self.subject,
        );
        false
    }

    /// Whether the environment declares this resource mandatory.
    fn required(&self) -> bool {
        required_from(std::env::var("CI").ok())
    }
}

/// Pure policy behind [`Gate::required`], factored out so every case
/// can be tested without mutating process-global environment variables:
/// a non-blank `ci` — the variable every CI provider sets — makes the
/// resource mandatory, and nothing else does.
fn required_from(ci: Option<String>) -> bool {
    ci.is_some_and(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_GATE: Gate = Gate::new("Widgetron", "Widgetron runtime", "widget test");

    /// Any non-blank value counts, the falsy words included: `CI` says
    /// where the run is, and has no spelling for "here, but optional".
    #[test]
    fn a_non_blank_ci_makes_the_resource_mandatory() {
        assert!(required_from(Some("true".into())));
        assert!(required_from(Some("1".into())));
        assert!(required_from(Some("false".into())));
    }

    #[test]
    fn an_absent_or_blank_ci_leaves_it_optional() {
        assert!(!required_from(None));
        assert!(!required_from(Some("".into())));
        assert!(!required_from(Some("  ".into())));
    }

    /// The optional half: the caller is told, and the test returns
    /// early. An implementation that stayed silent would look identical
    /// to a pass.
    #[test]
    fn an_optional_environment_skips_and_says_so() {
        assert!(!TEST_GATE.unavailable("nothing found", false));
    }

    /// The mandatory half: the panic has to say which resource and
    /// which variable made it mandatory, or the reader has neither the
    /// diagnosis nor the reason it was not skipped.
    #[test]
    #[should_panic(expected = "Widgetron is unavailable (nothing found)")]
    fn a_mandatory_environment_panics_instead_of_skipping() {
        TEST_GATE.unavailable("nothing found", true);
    }

    #[test]
    #[should_panic(expected = "and CI declares it mandatory here")]
    fn the_panic_names_the_variable_that_made_it_mandatory() {
        TEST_GATE.unavailable("nothing found", true);
    }

    /// The broken half: found and unusable fails even where an absence
    /// would be skipped. `required` is passed as `false` explicitly
    /// rather than read from the environment, so the test still
    /// discriminates on a CI runner, where `CI` alone would make any
    /// unavailable resource panic.
    #[test]
    #[should_panic(expected = "Widgetron was found but cannot be used (bad build)")]
    fn a_broken_resource_fails_even_where_it_is_optional() {
        TEST_GATE.resolve(Unavailable::Broken("bad build".into()), false);
    }

    #[test]
    #[should_panic(expected = "fails whether or not it is set")]
    fn a_broken_resource_says_that_ci_does_not_decide_it() {
        TEST_GATE.resolve(Unavailable::Broken("bad build".into()), false);
    }

    /// The absent half still goes through the require flag when it
    /// arrives as an [`Unavailable`].
    #[test]
    fn an_absent_resource_still_skips_where_it_is_optional() {
        assert!(!TEST_GATE.resolve(Unavailable::Absent("nothing found".into()), false));
    }

    /// A present resource is unaffected by the require flag: the flag
    /// governs what an absence means, not whether the probe runs.
    #[test]
    fn a_present_resource_is_not_affected_by_the_require_flag() {
        assert!(TEST_GATE.check(|| Ok(())));
    }
}
