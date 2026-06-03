// SPDX-License-Identifier: Apache-2.0

//! Publisher-name evaluators.
//!
//! Two independent signals, each addressed by audit's publisher row:
//!
//! - **Whitelist**: a curated list of reputable imprints, matched
//!   after light normalisation (case, punctuation, common
//!   abbreviations). The list itself is data, loaded at runtime from
//!   `publishers.toml` via [`crate::rules::AuditRules`]. A miss is
//!   always neutral — long-tail and unconfigured publishers stay
//!   uncovered.
//!
//! - **Shape sniff**: rejects values that look structurally like
//!   distribution watermarks rather than publisher names. The
//!   closed-form patterns (URLs, emails, common TLDs) live in this
//!   module; the token lists (contact handles, promo verbs, channel
//!   brands, CJK fragments) are data, loaded alongside the
//!   whitelist.

use crate::rules::AuditRules;

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

/// Evaluate one publisher value against the loaded rule set.
///
/// `url_watermark` gates the closed-form URL / email shape sniff
/// (R-18). `normalise_abbreviations` gates the abbreviation expansion
/// step in whitelist matching (R-19). Token lists in `rules` are
/// orthogonal — they are pure data and are always consulted.
pub fn evaluate(
    value: &str,
    rules: &AuditRules,
    url_watermark: bool,
    normalise_abbreviations: bool,
) -> PublisherVerdict {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return PublisherVerdict::Neutral;
    }
    if looks_like_watermark(trimmed, rules, url_watermark) {
        return PublisherVerdict::Watermark;
    }
    if is_whitelisted(trimmed, rules, normalise_abbreviations) {
        return PublisherVerdict::Whitelisted;
    }
    PublisherVerdict::Neutral
}

/// True when the value carries any watermark / contact / promo
/// pattern. The structural patterns (URL, email, TLD suffixes) are
/// pre-decided here and gated by `url_watermark`; the token lists are
/// read from `rules` and always consulted.
fn looks_like_watermark(value: &str, rules: &AuditRules, url_watermark: bool) -> bool {
    let lower: String = value.to_lowercase();
    if url_watermark {
        if lower.contains("http://")
            || lower.contains("https://")
            || lower.contains("www.")
            || lower.contains(".com")
            || lower.contains(".net")
            || lower.contains(".org")
            || lower.contains(".cn")
        {
            return true;
        }
        if lower.contains('@') {
            return true;
        }
    }
    for token in &rules.contact_tokens {
        if lower.contains(&token.to_lowercase()) {
            return true;
        }
    }
    for token in &rules.promo_tokens {
        if lower.contains(&token.to_lowercase()) {
            return true;
        }
    }
    for token in &rules.ascii_distribution_tokens {
        if lower.contains(&token.to_lowercase()) {
            return true;
        }
    }
    // CJK tokens match against the original value because
    // `to_lowercase()` would leave them unchanged anyway and we want
    // the substring check to run against the same bytes the user
    // configured.
    for token in &rules.watermark_cjk_tokens {
        if value.contains(token.as_str()) {
            return true;
        }
    }
    false
}

/// True when the value, after normalisation, matches the loaded
/// whitelist. `expand_abbrev` controls whether the abbreviation pass
/// runs on both sides of the comparison.
fn is_whitelisted(value: &str, rules: &AuditRules, expand_abbrev: bool) -> bool {
    let normalised = normalise(value, expand_abbrev);
    rules
        .publisher_whitelist
        .iter()
        .any(|candidate| normalise(candidate, expand_abbrev) == normalised)
}

/// Normalise a publisher name for whitelist comparison: lowercase,
/// drop punctuation silently, optionally expand a small set of common
/// abbreviations, collapse runs of whitespace.
///
/// Punctuation is dropped without inserting a space so that the dotted
/// form (`M.I.T.`) and the run-together form (`MIT`) normalise
/// identically. Whitespace is the only token-splitter.
fn normalise(value: &str, expand_abbrev: bool) -> String {
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
        expand_abbreviations(&out)
    } else {
        out
    }
}

/// Expand a short, hand-picked list of abbreviations whose absence
/// would create false misses against typical whitelist entries.
fn expand_abbreviations(value: &str) -> String {
    let mut tokens: Vec<String> = value.split_whitespace().map(str::to_string).collect();
    for token in &mut tokens {
        if token == "univ" {
            *token = "university".to_string();
        } else if token == "publ" || token == "pub" {
            *token = "publishing".to_string();
        } else if token == "co" {
            *token = "company".to_string();
        } else if token == "intl" {
            *token = "international".to_string();
        }
    }
    tokens.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules_with_whitelist(entries: &[&str]) -> AuditRules {
        AuditRules {
            publisher_whitelist: entries.iter().map(|s| (*s).to_string()).collect(),
            ..AuditRules::empty()
        }
    }

    fn rules_with_contact(tokens: &[&str]) -> AuditRules {
        AuditRules {
            contact_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
            ..AuditRules::empty()
        }
    }

    fn rules_with_promo(tokens: &[&str]) -> AuditRules {
        AuditRules {
            promo_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
            ..AuditRules::empty()
        }
    }

    fn rules_with_ascii_distribution(tokens: &[&str]) -> AuditRules {
        AuditRules {
            ascii_distribution_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
            ..AuditRules::empty()
        }
    }

    fn rules_with_cjk(tokens: &[&str]) -> AuditRules {
        AuditRules {
            watermark_cjk_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
            ..AuditRules::empty()
        }
    }

    #[test]
    fn whitelist_matches_with_punctuation_and_case() {
        let rules = rules_with_whitelist(&["Oxford University Press", "MIT Press"]);
        assert_eq!(
            evaluate("oxford university press", &rules, true, true),
            PublisherVerdict::Whitelisted
        );
        assert_eq!(
            evaluate("Oxford Univ. Press", &rules, true, true),
            PublisherVerdict::Whitelisted
        );
        assert_eq!(
            evaluate("M.I.T. Press", &rules, true, true),
            PublisherVerdict::Whitelisted
        );
    }

    #[test]
    fn url_value_flagged_as_watermark() {
        let rules = AuditRules::empty();
        assert_eq!(
            evaluate("https://example.com/free-ebooks", &rules, true, true),
            PublisherVerdict::Watermark
        );
        assert_eq!(
            evaluate("www.example.net", &rules, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn email_value_flagged_as_watermark() {
        let rules = AuditRules::empty();
        assert_eq!(
            evaluate("contact: test@example.net", &rules, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn contact_token_flagged_as_watermark() {
        let rules = rules_with_contact(&["qq:"]);
        assert_eq!(
            evaluate("scanned by anon, qq: 1234", &rules, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn promo_verb_flagged_as_watermark() {
        let rules = rules_with_promo(&["free ebook"]);
        assert_eq!(
            evaluate("free ebook download", &rules, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn ascii_distribution_handle_flagged_as_watermark() {
        let rules = rules_with_ascii_distribution(&["acme-rip"]);
        assert_eq!(
            evaluate("acme-rip", &rules, true, true),
            PublisherVerdict::Watermark
        );
        // Case-insensitive substring.
        assert_eq!(
            evaluate("ACME-RIP edition", &rules, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn cjk_token_flagged_as_watermark() {
        // "ce shi" (test placeholder) — never a real watermark token,
        // but exercises the CJK substring path. `\u{...}` escapes keep
        // the source bytes ASCII per repo policy.
        let token = "\u{6D4B}\u{8BD5}";
        let rules = rules_with_cjk(&[token]);
        let input = format!("prefix {token} suffix");
        assert_eq!(
            evaluate(&input, &rules, true, true),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn long_tail_value_stays_neutral_with_empty_rules() {
        let rules = AuditRules::empty();
        assert_eq!(
            evaluate("Independent Curiosities Press", &rules, true, true),
            PublisherVerdict::Neutral
        );
    }

    #[test]
    fn empty_value_is_neutral() {
        let rules = AuditRules::empty();
        assert_eq!(evaluate("", &rules, true, true), PublisherVerdict::Neutral);
        assert_eq!(
            evaluate("   ", &rules, true, true),
            PublisherVerdict::Neutral
        );
    }
}
