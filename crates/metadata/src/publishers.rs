// SPDX-License-Identifier: Apache-2.0

//! Publisher-name evaluators.
//!
//! Two independent signals, each addressed by audit's publisher row:
//!
//! - **Whitelist**: a curated list of reputable imprints, matched
//!   after light normalisation (case, punctuation, common
//!   abbreviations). The list itself is data, loaded at runtime from
//!   `audit_data.toml` via [`crate::AuditData`]. A miss is always
//!   neutral — long-tail and unconfigured publishers stay uncovered.
//!
//! - **Shape sniff**: rejects values that look structurally like
//!   distribution watermarks rather than publisher names. Every
//!   substring pattern (URL forms, email markers, contact handles,
//!   promo verbs, channel brands, CJK fragments) is data, loaded
//!   alongside the whitelist; the abbreviation expansion map applied
//!   during whitelist comparison is data too.
//!
//! Each verdict carries the specific sub-rule that matched, so an
//! audit consumer can attribute the decision to a named rule without
//! re-deriving the comparison.

use crate::AuditData;

/// Namespaced rule identifiers attached to publisher audit flags.
///
/// Strings are emitted verbatim in [`crate::report::Flag::PublisherRuleHit`]
/// tokens. New whitelist or watermark families add a constant here
/// before they appear in a flag.
pub mod rules {
    /// Whitelist match where the value, lowercased, equals a
    /// lowercased whitelist entry — no further normalisation needed.
    pub const WHITELIST_EXACT_LOWER: &str = "publisher:whitelist_exact_lower";
    /// Whitelist match that required punctuation drop and whitespace
    /// collapse to align with a whitelist entry.
    pub const WHITELIST_NORMALIZED: &str = "publisher:whitelist_normalized";
    /// Whitelist match that required the abbreviation-expansion pass
    /// on top of the normalisation step.
    pub const WHITELIST_ABBREV_EXPAND: &str = "publisher:whitelist_abbrev_expand";
    /// Watermark hit on the closed-form URL substring list.
    pub const WATERMARK_URL_SUBSTRING: &str = "publisher:watermark_url_substring";
    /// Watermark hit on the closed-form e-mail substring list.
    pub const WATERMARK_EMAIL_SUBSTRING: &str = "publisher:watermark_email_substring";
    /// Watermark hit on the operator-curated contact-token list.
    pub const WATERMARK_CONTACT_TOKEN: &str = "publisher:watermark_contact_token";
    /// Watermark hit on the operator-curated promo-verb list.
    pub const WATERMARK_PROMO_TOKEN: &str = "publisher:watermark_promo_token";
    /// Watermark hit on the operator-curated ASCII distribution-handle list.
    pub const WATERMARK_ASCII_DISTRIBUTION: &str = "publisher:watermark_ascii_distribution";
    /// Watermark hit on the operator-curated CJK-token list.
    pub const WATERMARK_CJK_TOKEN: &str = "publisher:watermark_cjk_token";
}

/// Which whitelist comparison path produced the match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhitelistMatch {
    /// The value's lowercase form equals a whitelist entry's lowercase form.
    ExactLower,
    /// The match needed punctuation drop and whitespace collapse.
    Normalized,
    /// The match additionally required the abbreviation-expansion pass.
    AbbrevExpand,
}

impl WhitelistMatch {
    /// Namespaced rule identifier for this match path.
    pub fn rule(self) -> &'static str {
        match self {
            WhitelistMatch::ExactLower => rules::WHITELIST_EXACT_LOWER,
            WhitelistMatch::Normalized => rules::WHITELIST_NORMALIZED,
            WhitelistMatch::AbbrevExpand => rules::WHITELIST_ABBREV_EXPAND,
        }
    }
}

/// Which watermark family the value matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatermarkKind {
    /// Closed-form URL substring (e.g. `http://`, `www.`).
    UrlSubstring,
    /// Closed-form e-mail substring (e.g. `@`, `mailto:`).
    EmailSubstring,
    /// Operator-curated contact-token substring (e.g. `qq:`).
    ContactToken,
    /// Operator-curated promo-verb substring (e.g. `free ebook`).
    PromoToken,
    /// Operator-curated ASCII distribution-handle substring.
    AsciiDistribution,
    /// Operator-curated CJK-token substring.
    CjkToken,
}

impl WatermarkKind {
    /// Namespaced rule identifier for this watermark family.
    pub fn rule(self) -> &'static str {
        match self {
            WatermarkKind::UrlSubstring => rules::WATERMARK_URL_SUBSTRING,
            WatermarkKind::EmailSubstring => rules::WATERMARK_EMAIL_SUBSTRING,
            WatermarkKind::ContactToken => rules::WATERMARK_CONTACT_TOKEN,
            WatermarkKind::PromoToken => rules::WATERMARK_PROMO_TOKEN,
            WatermarkKind::AsciiDistribution => rules::WATERMARK_ASCII_DISTRIBUTION,
            WatermarkKind::CjkToken => rules::WATERMARK_CJK_TOKEN,
        }
    }
}

/// Decision the publisher evaluator returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublisherVerdict {
    /// The value matched the curated whitelist of reputable imprints,
    /// carrying the path that produced the match.
    Whitelisted { match_kind: WhitelistMatch },
    /// The value's shape looks like a watermark, carrying the family
    /// that fired.
    Watermark { kind: WatermarkKind },
    /// Neither signal fired — neutral.
    Neutral,
}

/// Evaluate one publisher value against the loaded data set.
///
/// `url_watermark` gates the closed-form URL / email shape sniff
/// (R-18). `normalise_abbreviations` gates the abbreviation expansion
/// step in whitelist matching (R-19). The operator-curated token lists
/// in `data` are orthogonal — always consulted.
pub fn evaluate(
    value: &str,
    data: &AuditData,
    url_watermark: bool,
    normalise_abbreviations: bool,
) -> PublisherVerdict {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return PublisherVerdict::Neutral;
    }
    if let Some(kind) = classify_watermark(trimmed, data, url_watermark) {
        return PublisherVerdict::Watermark { kind };
    }
    if let Some(match_kind) = classify_whitelist_match(trimmed, data, normalise_abbreviations) {
        return PublisherVerdict::Whitelisted { match_kind };
    }
    PublisherVerdict::Neutral
}

/// Return the first watermark family that matches the value, or
/// `None`. The closed-form URL and e-mail substrings are gated by
/// `url_watermark`; the operator-curated token lists are always
/// consulted.
fn classify_watermark(value: &str, data: &AuditData, url_watermark: bool) -> Option<WatermarkKind> {
    let lower: String = value.to_lowercase();
    if url_watermark {
        for needle in &data.watermark_url_substrings {
            if !needle.is_empty() && lower.contains(&needle.to_lowercase()) {
                return Some(WatermarkKind::UrlSubstring);
            }
        }
        for needle in &data.watermark_email_substrings {
            if !needle.is_empty() && lower.contains(&needle.to_lowercase()) {
                return Some(WatermarkKind::EmailSubstring);
            }
        }
    }
    for token in &data.contact_tokens {
        if !token.is_empty() && lower.contains(&token.to_lowercase()) {
            return Some(WatermarkKind::ContactToken);
        }
    }
    for token in &data.promo_tokens {
        if !token.is_empty() && lower.contains(&token.to_lowercase()) {
            return Some(WatermarkKind::PromoToken);
        }
    }
    for token in &data.ascii_distribution_tokens {
        if !token.is_empty() && lower.contains(&token.to_lowercase()) {
            return Some(WatermarkKind::AsciiDistribution);
        }
    }
    // CJK tokens match against the original value because
    // `to_lowercase()` would leave them unchanged anyway and we want
    // the substring check to run against the same bytes the user
    // configured.
    for token in &data.watermark_cjk_tokens {
        if !token.is_empty() && value.contains(token.as_str()) {
            return Some(WatermarkKind::CjkToken);
        }
    }
    None
}

/// Classify how the value matches the whitelist, if at all. Tries the
/// strictest path first so the returned variant identifies the lightest
/// normalisation that sufficed: lowercase equality, then punctuation
/// drop and whitespace collapse, then (when enabled) abbreviation
/// expansion.
fn classify_whitelist_match(
    value: &str,
    data: &AuditData,
    expand_abbrev: bool,
) -> Option<WhitelistMatch> {
    let value_lower = value.to_lowercase();
    if data
        .publisher_whitelist
        .iter()
        .any(|candidate| candidate.to_lowercase() == value_lower)
    {
        return Some(WhitelistMatch::ExactLower);
    }
    let value_normalised = normalise(value, &data.abbreviations, false);
    if data
        .publisher_whitelist
        .iter()
        .any(|candidate| normalise(candidate, &data.abbreviations, false) == value_normalised)
    {
        return Some(WhitelistMatch::Normalized);
    }
    if expand_abbrev {
        let value_expanded = normalise(value, &data.abbreviations, true);
        if data
            .publisher_whitelist
            .iter()
            .any(|candidate| normalise(candidate, &data.abbreviations, true) == value_expanded)
        {
            return Some(WhitelistMatch::AbbrevExpand);
        }
    }
    None
}

/// Normalise a publisher name for whitelist comparison: lowercase,
/// drop punctuation silently, optionally expand the configured
/// abbreviation map, collapse runs of whitespace.
///
/// Punctuation is dropped without inserting a space so that the dotted
/// form (`M.I.T.`) and the run-together form (`MIT`) normalise
/// identically. Whitespace is the only token-splitter.
fn normalise(
    value: &str,
    abbreviations: &std::collections::BTreeMap<String, String>,
    expand_abbrev: bool,
) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_space = true;
    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_alphanumeric() {
            out.push(ch);
            last_space = false;
        } else if ch.is_whitespace() && !last_space && !out.is_empty() {
            out.push(' ');
            last_space = true;
        }
        // Other characters (punctuation, symbols) are dropped silently.
    }
    if out.ends_with(' ') {
        out.pop();
    }
    if expand_abbrev {
        expand_abbreviations(&out, abbreviations)
    } else {
        out
    }
}

/// Expand each whitespace-bounded token through the abbreviation map;
/// tokens absent from the map ride through unchanged.
fn expand_abbreviations(
    value: &str,
    abbreviations: &std::collections::BTreeMap<String, String>,
) -> String {
    value
        .split_whitespace()
        .map(|tok| {
            abbreviations
                .get(tok)
                .map(String::as_str)
                .unwrap_or(tok)
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_with_whitelist(entries: &[&str]) -> AuditData {
        AuditData {
            publisher_whitelist: entries.iter().map(|s| (*s).to_string()).collect(),
            abbreviations: AuditData::default_data().abbreviations,
            ..AuditData::empty()
        }
    }

    fn data_with_contact(tokens: &[&str]) -> AuditData {
        AuditData {
            contact_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
            ..AuditData::empty()
        }
    }

    fn data_with_promo(tokens: &[&str]) -> AuditData {
        AuditData {
            promo_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
            ..AuditData::empty()
        }
    }

    fn data_with_ascii_distribution(tokens: &[&str]) -> AuditData {
        AuditData {
            ascii_distribution_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
            ..AuditData::empty()
        }
    }

    fn data_with_cjk(tokens: &[&str]) -> AuditData {
        AuditData {
            watermark_cjk_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
            ..AuditData::empty()
        }
    }

    /// Shipped default URL / e-mail substrings, used by the watermark
    /// tests that exercise the closed-form sniff path.
    fn data_with_default_url_patterns() -> AuditData {
        let mut data = AuditData::empty();
        let defaults = AuditData::default_data();
        data.watermark_url_substrings = defaults.watermark_url_substrings;
        data.watermark_email_substrings = defaults.watermark_email_substrings;
        data
    }

    #[test]
    fn whitelist_exact_lower_path() {
        let data = data_with_whitelist(&["Oxford University Press"]);
        assert_eq!(
            evaluate("oxford university press", &data, true, true),
            PublisherVerdict::Whitelisted {
                match_kind: WhitelistMatch::ExactLower
            }
        );
    }

    #[test]
    fn whitelist_normalized_path_handles_punctuation() {
        let data = data_with_whitelist(&["MIT Press"]);
        assert_eq!(
            evaluate("M.I.T. Press", &data, true, true),
            PublisherVerdict::Whitelisted {
                match_kind: WhitelistMatch::Normalized
            }
        );
    }

    #[test]
    fn whitelist_abbrev_expand_path() {
        let data = data_with_whitelist(&["Oxford University Press"]);
        assert_eq!(
            evaluate("Oxford Univ. Press", &data, true, true),
            PublisherVerdict::Whitelisted {
                match_kind: WhitelistMatch::AbbrevExpand
            }
        );
        // Same input without the expansion pass falls through to
        // neutral — the punctuation-dropped form `oxford univ press`
        // does not equal `oxford university press` on its own.
        assert_eq!(
            evaluate("Oxford Univ. Press", &data, true, false),
            PublisherVerdict::Neutral
        );
    }

    #[test]
    fn url_value_flagged_as_watermark() {
        let data = data_with_default_url_patterns();
        assert_eq!(
            evaluate("https://example.com/free-ebooks", &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::UrlSubstring
            }
        );
        assert_eq!(
            evaluate("www.example.net", &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::UrlSubstring
            }
        );
    }

    #[test]
    fn email_value_flagged_as_watermark() {
        let data = data_with_default_url_patterns();
        // Bare `@` form with no TLD substring, so only the email rule
        // can fire — the URL list is consulted first and would otherwise
        // claim values like `name@example.net` through `.net`.
        assert_eq!(
            evaluate("contact: editor@somewhere", &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::EmailSubstring
            }
        );
    }

    #[test]
    fn contact_token_flagged_as_watermark() {
        let data = data_with_contact(&["qq:"]);
        assert_eq!(
            evaluate("scanned by anon, qq: 1234", &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::ContactToken
            }
        );
    }

    #[test]
    fn promo_verb_flagged_as_watermark() {
        let data = data_with_promo(&["free ebook"]);
        assert_eq!(
            evaluate("free ebook download", &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::PromoToken
            }
        );
    }

    #[test]
    fn ascii_distribution_handle_flagged_as_watermark() {
        let data = data_with_ascii_distribution(&["acme-rip"]);
        assert_eq!(
            evaluate("acme-rip", &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::AsciiDistribution
            }
        );
        // Case-insensitive substring.
        assert_eq!(
            evaluate("ACME-RIP edition", &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::AsciiDistribution
            }
        );
    }

    #[test]
    fn cjk_token_flagged_as_watermark() {
        // "ce shi" (test placeholder) — never a real watermark token,
        // but exercises the CJK substring path. `\u{...}` escapes keep
        // the source bytes ASCII per repo policy.
        let token = "\u{6D4B}\u{8BD5}";
        let data = data_with_cjk(&[token]);
        let input = format!("prefix {token} suffix");
        assert_eq!(
            evaluate(&input, &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::CjkToken
            }
        );
    }

    #[test]
    fn blank_token_does_not_flag_every_value() {
        // A stray empty entry in any substring list would make
        // `contains("")` fire for every value; the guard keeps a normal
        // publisher neutral while a real token in the same list still hits.
        let data = AuditData {
            contact_tokens: vec![String::new(), "qq:".to_string()],
            promo_tokens: vec![String::new()],
            ascii_distribution_tokens: vec![String::new()],
            watermark_cjk_tokens: vec![String::new()],
            ..AuditData::empty()
        };
        assert_eq!(
            evaluate("Oxford University Press", &data, true, true),
            PublisherVerdict::Neutral
        );
        assert_eq!(
            evaluate("scanned by anon, qq: 1234", &data, true, true),
            PublisherVerdict::Watermark {
                kind: WatermarkKind::ContactToken
            }
        );
    }

    #[test]
    fn long_tail_value_stays_neutral_with_empty_data() {
        let data = AuditData::empty();
        assert_eq!(
            evaluate("Independent Curiosities Press", &data, true, true),
            PublisherVerdict::Neutral
        );
    }

    #[test]
    fn empty_value_is_neutral() {
        let data = AuditData::empty();
        assert_eq!(evaluate("", &data, true, true), PublisherVerdict::Neutral);
        assert_eq!(
            evaluate("   ", &data, true, true),
            PublisherVerdict::Neutral
        );
    }

    #[test]
    fn whitelist_match_rule_names_round_trip() {
        assert_eq!(
            WhitelistMatch::ExactLower.rule(),
            rules::WHITELIST_EXACT_LOWER
        );
        assert_eq!(
            WhitelistMatch::Normalized.rule(),
            rules::WHITELIST_NORMALIZED
        );
        assert_eq!(
            WhitelistMatch::AbbrevExpand.rule(),
            rules::WHITELIST_ABBREV_EXPAND
        );
    }

    #[test]
    fn watermark_kind_rule_names_round_trip() {
        assert_eq!(
            WatermarkKind::UrlSubstring.rule(),
            rules::WATERMARK_URL_SUBSTRING
        );
        assert_eq!(
            WatermarkKind::EmailSubstring.rule(),
            rules::WATERMARK_EMAIL_SUBSTRING
        );
        assert_eq!(
            WatermarkKind::ContactToken.rule(),
            rules::WATERMARK_CONTACT_TOKEN
        );
        assert_eq!(
            WatermarkKind::PromoToken.rule(),
            rules::WATERMARK_PROMO_TOKEN
        );
        assert_eq!(
            WatermarkKind::AsciiDistribution.rule(),
            rules::WATERMARK_ASCII_DISTRIBUTION
        );
        assert_eq!(WatermarkKind::CjkToken.rule(), rules::WATERMARK_CJK_TOKEN);
    }
}
