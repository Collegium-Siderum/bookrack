// SPDX-License-Identifier: Apache-2.0

//! What a rule file would produce, worked out without a library.

use std::path::Path;

use crate::pipeline::book::{AuditData, AuditProfile, DryrunParams};

use bookrack_audit_profile::HeadingPatterns;

pub use crate::pipeline::book::{RuleCompletenessOut, RuleDryrunReport, RuleFallbackOut};

/// Dryrun the book the rule file at `rule` describes, auditing it under
/// `audit_profile` with `audit_data` and reading headings by
/// `heading_patterns`.
///
/// Opens no database and writes nothing. A rule that a submission would
/// refuse is refused here with the same error in the chain.
pub fn dryrun(
    rule: &Path,
    audit_data: AuditData,
    audit_profile: AuditProfile,
    heading_patterns: HeadingPatterns,
) -> eyre::Result<RuleDryrunReport> {
    let params = DryrunParams {
        audit_data,
        audit_profile,
        heading_patterns,
        ..DryrunParams::default()
    };
    Ok(crate::pipeline::book::dryrun_rule(rule, &params)?)
}
