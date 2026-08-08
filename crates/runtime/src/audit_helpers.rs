// SPDX-License-Identifier: Apache-2.0

//! Audit-rule loaders shared by every command that builds an
//! `IngestParams`. Each loader falls back to the shipped default on a
//! missing or malformed overlay so a partial install does not refuse
//! to start.

use bookrack_config::Config;
use bookrack_glean::audit::{
    PROFILE_DEFAULT as PAPER_PROFILE_DEFAULT, PROFILE_STRICT as PAPER_PROFILE_STRICT,
    PROFILE_TRUST_SOURCE as PAPER_PROFILE_TRUST_SOURCE, PaperAuditData, PaperAuditProfile,
};
use bookrack_metadata::AuditData;

use crate::cmd::input_error::CmdInputError;

/// Refuse an audit-profile name that matches no built-in in `names`.
///
/// `None` keeps its meaning — the overlay-resolved default — so this
/// checks what the caller wrote rather than requiring them to write
/// anything.
///
/// The built-in set is a parameter, not a constant this function
/// picks. The book side is `bookrack_audit_profile::ALL_BUILT_IN_NAMES`
/// and the paper side `bookrack_glean::audit::profile::ALL_BUILT_IN_NAMES`;
/// the two hold the same three names today but are separate constants,
/// and a function that chose one of them would start rejecting legal
/// names on the other side the day it grows a profile — silently, with
/// the tests still green.
pub fn require_known_profile(name: Option<&str>, names: &[&str]) -> Result<(), CmdInputError> {
    match name {
        None => Ok(()),
        Some(n) if names.contains(&n) => Ok(()),
        Some(n) => Err(CmdInputError::BadArgument {
            arg: "audit_profile",
            value: n.to_string(),
            expected: names.join(", "),
        }),
    }
}

/// Load the metadata audit's runtime data set from
/// `cfg.audit_rules_dir()`. A missing directory or malformed file is
/// logged and the shipped default is returned, so the audit still
/// runs against the in-repo URL / abbreviation / placeholder / extension
/// defaults; only the operator-curated token lists fall back to empty.
pub fn load_audit_data(cfg: &Config) -> AuditData {
    match AuditData::load_from(&cfg.audit_rules_dir()) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load audit data overlay; using shipped default",
            );
            AuditData::default_data()
        }
    }
}

/// Load the multi-language heading patterns from
/// `cfg.audit_rules_dir()`. A missing directory or malformed file is
/// logged and the shipped default is returned.
pub fn load_heading_patterns(cfg: &Config) -> bookrack_audit_profile::HeadingPatterns {
    match bookrack_audit_profile::HeadingPatterns::load_from(&cfg.audit_rules_dir()) {
        Ok(patterns) => patterns,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load heading patterns overlay; using shipped default",
            );
            bookrack_audit_profile::HeadingPatterns::default_patterns()
        }
    }
}

/// Resolve the active audit profile.
///
/// When `name` is `Some`, the named built-in (`default` /
/// `trust-source` / `strict`) is returned; an unknown name falls
/// through to the overlay path. When `name` is `None`, the shipped
/// default is loaded and merged with any
/// `<data_root>/audit-rules/audit_profile.local.toml` overlay. A
/// malformed overlay is logged and the in-repo default is used as-is.
///
/// The unknown-name fallback is reachable only in-process: every
/// control-plane entry point runs [`require_known_profile`] before
/// calling this, so a name that came off the wire is either a built-in
/// or already refused.
pub fn load_audit_profile(cfg: &Config, name: Option<&str>) -> bookrack_metadata::AuditProfile {
    if let Some(label) = name
        && let Some(named) = bookrack_metadata::AuditProfile::from_named(label)
    {
        return named;
    }
    match bookrack_metadata::AuditProfile::load_from(&cfg.audit_rules_dir()) {
        Ok(profile) => profile,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load audit profile overlay; using shipped default",
            );
            bookrack_metadata::AuditProfile::default_profile()
        }
    }
}

/// Resolve the paper-side audit profile.
///
/// When `name` is `Some`, the named built-in (`default` /
/// `trust-source` / `strict`) is returned; an unknown name falls
/// through to the overlay path. When `name` is `None`, the shipped
/// default is loaded and merged with any
/// `<data_root>/audit-rules/paper_audit_profile.local.toml` overlay.
/// A malformed overlay is logged and the in-repo default is used as-is.
///
/// As on the book side, the unknown-name fallback is reachable only
/// in-process: the control-plane entry point runs
/// [`require_known_profile`] against the paper-side built-in set
/// first.
pub fn load_paper_audit_profile(cfg: &Config, name: Option<&str>) -> PaperAuditProfile {
    if let Some(label) = name {
        match label {
            PAPER_PROFILE_DEFAULT => return PaperAuditProfile::default_profile(),
            PAPER_PROFILE_TRUST_SOURCE => return PaperAuditProfile::trust_source(),
            PAPER_PROFILE_STRICT => return PaperAuditProfile::strict(),
            _ => {}
        }
    }
    match PaperAuditProfile::load_from(&cfg.audit_rules_dir()) {
        Ok(profile) => profile,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load paper audit profile overlay; using shipped default",
            );
            PaperAuditProfile::default_profile()
        }
    }
}

/// Load the paper-side audit data set from `cfg.audit_rules_dir()`.
/// A missing directory or malformed file is logged and the shipped
/// default is returned.
pub fn load_paper_audit_data(cfg: &Config) -> PaperAuditData {
    match PaperAuditData::load_from(&cfg.audit_rules_dir()) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load paper audit data overlay; using shipped default",
            );
            PaperAuditData::default_data()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The accepted set is written out rather than read from
    /// `ALL_BUILT_IN_NAMES`: a test that consults the same constant the
    /// function does asserts nothing about the function. That the
    /// constant holds these three names is pinned in the crate that
    /// owns it.
    const NAMES: &[&str] = &["default", "trust-source", "strict"];

    #[test]
    fn a_built_in_name_passes() {
        assert!(require_known_profile(Some("strict"), NAMES).is_ok());
    }

    /// `None` is the overlay-resolved default, not a missing value the
    /// caller must supply.
    #[test]
    fn an_absent_name_passes() {
        assert!(require_known_profile(None, NAMES).is_ok());
    }

    #[test]
    fn a_near_miss_is_refused_and_names_both_sides() {
        let err = require_known_profile(Some("strictt"), NAMES)
            .expect_err("a name outside the set must be refused");
        let problem = bookrack_core::Explain::explain(&err);
        assert!(
            problem.summary.contains("\"strictt\""),
            "the refusal must quote what was sent: {}",
            problem.summary
        );
        let detail = problem
            .data
            .detail
            .expect("the accepted set is what makes the refusal actionable");
        for name in NAMES {
            assert!(detail.contains(name), "{detail} omits {name}");
        }
    }

    /// The set is a parameter, so a name legal on one side and unknown
    /// on the other is refused on the side that does not have it. This
    /// is what a shared hard-coded list would silently get wrong once
    /// the two sides diverge.
    #[test]
    fn the_accepted_set_is_the_one_that_was_passed_in() {
        assert!(require_known_profile(Some("paper-only"), NAMES).is_err());
        assert!(require_known_profile(Some("paper-only"), &["paper-only"]).is_ok());
    }
}
