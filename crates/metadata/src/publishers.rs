// SPDX-License-Identifier: Apache-2.0

//! Publisher-name evaluators.
//!
//! Two independent signals, each addressed by audit's publisher row:
//!
//! - **Whitelist**: a curated list of reputable imprints, matched after
//!   light normalisation (case, punctuation, common abbreviations).
//!   v1 ships the *mechanism*; the data list is intentionally a tiny
//!   seed and grows in dedicated data PRs without changing the engine.
//!   A miss is always neutral — long-tail publishers stay uncovered.
//!
//! - **Shape sniff**: rejects values that look structurally like
//!   distribution watermarks rather than publisher names — anything
//!   carrying URLs, contact tokens, or promotional verbs. Closed-form
//!   patterns, no list to maintain; effective day one.

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

/// Evaluate one publisher value.
pub fn evaluate(value: &str) -> PublisherVerdict {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return PublisherVerdict::Neutral;
    }
    if looks_like_watermark(trimmed) {
        return PublisherVerdict::Watermark;
    }
    if is_whitelisted(trimmed) {
        return PublisherVerdict::Whitelisted;
    }
    PublisherVerdict::Neutral
}

/// True when the value carries any of the watermark/contact/promo
/// patterns. Each pattern is structural and pre-decided, not data.
fn looks_like_watermark(value: &str) -> bool {
    let lower: String = value.to_lowercase();
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
    // Contact / chat tokens.
    for token in CONTACT_TOKENS {
        if lower.contains(token) {
            return true;
        }
    }
    // Promotional verbs.
    for token in PROMO_TOKENS {
        if lower.contains(token) {
            return true;
        }
    }
    false
}

/// Common contact-channel tokens. ASCII lower-case fragments only;
/// matching is substring, so the patterns hit case-insensitively.
const CONTACT_TOKENS: &[&str] = &["qq:", "qq ", "wechat", "weixin", "telegram", "skype:"];

/// Common promotional verbs that indicate a distribution channel.
const PROMO_TOKENS: &[&str] = &[
    "download",
    "free ebook",
    "free book",
    "ebook download",
    "scanned by",
    "ripped by",
    "uploaded by",
];

/// True when the value, after normalisation, matches the curated
/// whitelist.
fn is_whitelisted(value: &str) -> bool {
    let normalised = normalise(value);
    WHITELIST_SEED
        .iter()
        .any(|candidate| normalise(candidate) == normalised)
}

/// Normalise a publisher name for whitelist comparison: lowercase,
/// drop punctuation silently, expand a small set of common
/// abbreviations, collapse runs of whitespace.
///
/// Punctuation is dropped without inserting a space so that the dotted
/// form (`M.I.T.`) and the run-together form (`MIT`) normalise
/// identically. Whitespace is the only token-splitter.
fn normalise(value: &str) -> String {
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
    expand_abbreviations(&out)
}

/// Expand a short, hand-picked list of abbreviations whose absence
/// would create false misses against the seed list.
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

/// The curated seed list of reputable imprints. Held as a constant
/// rather than read from a file: the data is public, evolves through
/// pull requests, and a static constant compiles into the engine
/// without an extra I/O surface. Empty / tiny is fine: a miss is
/// always neutral by design.
const WHITELIST_SEED: &[&str] = &[
    "Oxford University Press",
    "Cambridge University Press",
    "Harvard University Press",
    "Princeton University Press",
    "Yale University Press",
    "Stanford University Press",
    "MIT Press",
    "Penguin Random House",
    "Penguin Books",
    "Random House",
    "Springer",
    "Springer Nature",
    "Wiley",
    "Elsevier",
    "Routledge",
    "Bloomsbury",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_matches_with_punctuation_and_case() {
        assert_eq!(
            evaluate("oxford university press"),
            PublisherVerdict::Whitelisted
        );
        assert_eq!(
            evaluate("Oxford Univ. Press"),
            PublisherVerdict::Whitelisted
        );
        assert_eq!(evaluate("M.I.T. Press"), PublisherVerdict::Whitelisted);
    }

    #[test]
    fn url_value_flagged_as_watermark() {
        assert_eq!(
            evaluate("https://example.com/free-ebooks"),
            PublisherVerdict::Watermark
        );
        assert_eq!(evaluate("www.example.net"), PublisherVerdict::Watermark);
    }

    #[test]
    fn contact_token_flagged_as_watermark() {
        assert_eq!(
            evaluate("scanned by anon, qq: 1234"),
            PublisherVerdict::Watermark
        );
        assert_eq!(
            evaluate("contact: test@example.com"),
            PublisherVerdict::Watermark
        );
    }

    #[test]
    fn promo_verb_flagged_as_watermark() {
        assert_eq!(evaluate("free ebook download"), PublisherVerdict::Watermark);
    }

    #[test]
    fn long_tail_value_stays_neutral() {
        // A plausible small-press name that is not on the seed list.
        assert_eq!(
            evaluate("Independent Curiosities Press"),
            PublisherVerdict::Neutral
        );
    }

    #[test]
    fn empty_value_is_neutral() {
        assert_eq!(evaluate(""), PublisherVerdict::Neutral);
        assert_eq!(evaluate("   "), PublisherVerdict::Neutral);
    }
}
