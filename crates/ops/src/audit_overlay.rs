// SPDX-License-Identifier: Apache-2.0

//! Audit-rule loaders: a library's audit profile, audit data, and
//! heading patterns, each read from the overlays under its data root.
//! Every command that builds pipeline parameters, and every read that
//! grades a record the way a write would, loads through here. Each
//! loader falls back to the shipped default on a missing or malformed
//! overlay so a partial install does not refuse to start.

use bookrack_config::Config;
use bookrack_metadata::AuditData;

use crate::{
    OpsError, PAPER_PROFILE_DEFAULT, PAPER_PROFILE_STRICT, PAPER_PROFILE_TRUST_SOURCE,
    PaperAuditData, PaperAuditProfile,
};

/// Refuse an audit-profile name that matches no built-in in `names`.
///
/// `None` keeps its meaning — the overlay-resolved default — so this
/// checks what the caller wrote rather than requiring them to write
/// anything.
///
/// The built-in set is a parameter, not a constant this function
/// picks. The book side is `bookrack_audit_profile::ALL_BUILT_IN_NAMES`
/// and the paper side [`crate::PAPER_AUDIT_BUILT_IN_NAMES`];
/// the two hold the same three names today but are separate constants,
/// and a function that chose one of them would start rejecting legal
/// names on the other side the day it grows a profile — silently, with
/// the tests still green.
///
/// A refusal is [`OpsError::UnknownAuditProfile`], carrying the name
/// sent and the set it was measured against.
pub fn require_known_profile(name: Option<&str>, names: &[&str]) -> crate::Result<()> {
    match name {
        None => Ok(()),
        Some(n) if names.contains(&n) => Ok(()),
        Some(n) => Err(OpsError::UnknownAuditProfile {
            value: n.to_string(),
            expected: names.iter().map(|name| name.to_string()).collect(),
        }),
    }
}

/// Load the metadata audit's runtime data set from
/// `cfg.audit_rules_dir()`. A missing directory or malformed file is
/// logged and the shipped default is returned, so the audit still
/// runs against the in-repo URL / abbreviation / placeholder / extension
/// defaults; only the operator-curated token lists fall back to empty.
// Parallel implementation: runtime-audit-helpers.
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
// Parallel implementation: runtime-audit-helpers.
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
// Parallel implementation: runtime-audit-helpers.
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
// Parallel implementation: runtime-audit-helpers.
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
        let OpsError::UnknownAuditProfile { value, expected } = &err else {
            panic!("the refusal is the unknown-profile variant: {err:?}");
        };
        assert_eq!(value, "strictt");
        assert_eq!(expected, NAMES, "the set it was measured against");
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

    /// A configuration whose audit-rules directory holds `files`.
    fn library_with_rules(files: &[(&str, &str)]) -> (tempfile::TempDir, Config) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = Config::new(tmp.path().to_path_buf());
        std::fs::create_dir_all(cfg.audit_rules_dir()).expect("audit-rules dir");
        for (name, body) in files {
            std::fs::write(cfg.audit_rules_dir().join(name), body).expect("write overlay");
        }
        (tmp, cfg)
    }

    #[test]
    fn a_library_profile_overlay_is_what_the_unnamed_profile_loads() {
        let (_tmp, cfg) = library_with_rules(&[(
            "audit_profile.local.toml",
            "schema_version = 1\n\n[year]\nmin = 1100\nmax = 2200\n",
        )]);
        let loaded = load_audit_profile(&cfg, None);
        assert_eq!((loaded.year.min, loaded.year.max), (1100, 2200));
        assert_ne!(
            loaded,
            bookrack_metadata::AuditProfile::default_profile(),
            "the overlay must take effect"
        );
    }

    #[test]
    fn a_malformed_profile_overlay_falls_back_to_the_shipped_default() {
        let (_tmp, cfg) =
            library_with_rules(&[("audit_profile.local.toml", "schema_version = [not toml")]);
        assert_eq!(
            load_audit_profile(&cfg, None),
            bookrack_metadata::AuditProfile::default_profile()
        );
    }

    /// A named built-in wins over the library's overlay.
    #[test]
    fn a_named_built_in_ignores_the_library_overlay() {
        let (_tmp, cfg) = library_with_rules(&[(
            "audit_profile.local.toml",
            "schema_version = 1\n\n[year]\nmin = 1100\nmax = 2200\n",
        )]);
        assert_eq!(
            load_audit_profile(&cfg, Some("strict")),
            bookrack_metadata::AuditProfile::from_named("strict").expect("built-in")
        );
    }

    #[test]
    fn a_library_data_overlay_is_what_the_data_loader_returns() {
        let (_tmp, cfg) = library_with_rules(&[(
            "audit_data.toml",
            "schema_version = 1\n[publishers]\nwhitelist = [\"Acme University Press\"]\n",
        )]);
        assert_eq!(
            load_audit_data(&cfg).publisher_whitelist,
            ["Acme University Press"]
        );
    }

    #[test]
    fn a_malformed_data_overlay_falls_back_to_the_shipped_default() {
        let (_tmp, cfg) = library_with_rules(&[("audit_data.toml", "[[[ not toml")]);
        assert_eq!(load_audit_data(&cfg), AuditData::default_data());
    }

    /// A named paper built-in wins over the library's overlay: each of
    /// the two names answers with its own profile while an overlay that
    /// differs from both sits on disk.
    #[test]
    fn a_named_paper_built_in_ignores_the_library_overlay() {
        let (_tmp, cfg) = library_with_rules(&[(
            "paper_audit_profile.local.toml",
            "schema_version = 1\n\n[title]\nplaceholder_check = false\n",
        )]);
        let overlaid = load_paper_audit_profile(&cfg, None);
        assert!(
            !overlaid.title.placeholder_check,
            "the overlay must take effect on the unnamed profile"
        );
        assert_eq!(
            load_paper_audit_profile(&cfg, Some("default")),
            PaperAuditProfile::default_profile()
        );
        assert_ne!(PaperAuditProfile::default_profile(), overlaid);
        assert_eq!(
            load_paper_audit_profile(&cfg, Some("trust-source")),
            PaperAuditProfile::trust_source()
        );
        assert_ne!(PaperAuditProfile::trust_source(), overlaid);
    }
}
