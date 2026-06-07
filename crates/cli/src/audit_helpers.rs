// SPDX-License-Identifier: Apache-2.0

//! Audit-rule loaders shared by every command that builds an
//! `IngestParams`. Each loader falls back to the shipped default on a
//! missing or malformed overlay so a partial install does not refuse
//! to start.

use bookrack_config::Config;
use bookrack_metadata::AuditData;

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
