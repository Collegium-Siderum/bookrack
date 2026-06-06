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

use crate::AuditData;

/// Decision the publisher evaluator returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublisherVerdict {
    /// The value matched the curated whitelist of reputable imprints.
    Whitelisted,
    /// The value's shape looks like a watermark, not a publisher name.
    Watermark,
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
    if looks_like_watermark(trimmed, data, url_watermark) {
        return PublisherVerdict::Watermark;
    }
    if is_whitelisted(trimmed, data, normalise_abbreviations) {
        return PublisherVerdict::Whitelisted;
    }
    PublisherVerdict::Neutral
}

/// True when the value carries any watermark / contact / promo
/// pattern. The closed-form URL and e-mail substrings are gated by
/// `url_watermark`; the operator-curated token lists are always
/// consulted.
fn looks_like_watermark(value: &str, data: &AuditData, url_watermark: bool) -> bool {
    let lower: String = value.to_lowercase();
    if url_watermark {
        for needle in &data.watermark_url_substrings {
            if lower.contains(&needle.to_lowercase()) {
                return true;
            }
        }
        for needle in &data.watermark_email_substrings {
            if lower.contains(&needle.to_lowercase()) {
                return true;
            }
        }
    }
    for token in &data.contact_tokens {
        if lower.contains(&token.to_lowercase()) {
            return true;
        }
    }
    for token in &data.promo_tokens {
        if lower.contains(&token.to_lowercase()) {
            return true;
        }
    }
    for token in &data.ascii_distribution_tokens {
        if lower.contains(&token.to_lowercase()) {
            return true;
        }
    }
    // CJK tokens match against the original value because
    // `to_lowercase()` would leave them unchanged anyway and we want
    // the substring check to run against the same bytes the user
    // configured.
    for token in &data.watermark_cjk_tokens {
        if value.contains(token.as_str()) {
            return true;
        }
    }
    false
}

/// True when the value, after normalisation, matches the loaded
/// whitelist. `expand_abbrev` controls whether the abbreviation pass
/// runs on both sides of the comparison.
fn is_whitelisted(value: &str, data: &AuditData, expand_abbrev: bool) -> bool {
    let normalised = normalise(value, &data.abbreviations, expand_abbrev);
    data.publisher_whitelist
        .iter()
        .any(|candidate| normalise(candidate, &data.abbreviations, expand_abbrev) == normalised)
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
    fn whitelist_matches_with_punctuation_and_case() {
        let data = data_with_whitelist(&["Oxford University Press", "MIT Press"]);
        assert_eq!(
            evaluate("oxford university press", &data, true, true),
            PublisherVerdict::Whitelisted
        );
        assert_eq!(
            evaluate("Oxford Univ. Press", &data, true, true),
            PublisherVerdict::Whitelisted
        );
        assert_eq!(
            evaluate("M.I.T. Press", &data, true, true),
            PublisherVerdict::Whitelisted
        );
    }

    #[test]
    fn url_value_flagged_as_watermark() {
        let data = data_with_default_url_patterns();
        assert_eq!(
            evaluate("https://example.com/free-ebooks", &data, true, true),
            PublisherVerdict::Watermark
        );
        assert_eq!(
            evaluate("www.example.net", &data, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn email_value_flagged_as_watermark() {
        let data = data_with_default_url_patterns();
        assert_eq!(
            evaluate("contact: test@example.net", &data, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn contact_token_flagged_as_watermark() {
        let data = data_with_contact(&["qq:"]);
        assert_eq!(
            evaluate("scanned by anon, qq: 1234", &data, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn promo_verb_flagged_as_watermark() {
        let data = data_with_promo(&["free ebook"]);
        assert_eq!(
            evaluate("free ebook download", &data, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn ascii_distribution_handle_flagged_as_watermark() {
        let data = data_with_ascii_distribution(&["acme-rip"]);
        assert_eq!(
            evaluate("acme-rip", &data, true, true),
            PublisherVerdict::Watermark
        );
        // Case-insensitive substring.
        assert_eq!(
            evaluate("ACME-RIP edition", &data, true, true),
            PublisherVerdict::Watermark
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
            PublisherVerdict::Watermark
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
}
