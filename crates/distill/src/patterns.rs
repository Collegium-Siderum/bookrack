// SPDX-License-Identifier: Apache-2.0

//! Pattern references used by the splitter and extractor stages.
//!
//! [`PatternRef`] is what `book.toml` passes to stages like
//! `partition_body_around_match` and `extract_bracketed_tag`: either
//! one of a small set of bracketed-tag shapes or an explicit regex.
//! [`match_pattern`] runs the reference against an input string and
//! returns the matched span plus the inner capture, so stages can
//! both write the captured value into a payload key and strip the
//! match from the source text.

use regex::Regex;

/// The two bracket-shaped tag patterns books actually use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BracketKind {
    Angle,
    Square,
    Paren,
}

impl BracketKind {
    /// Regex capturing the inner contents of the bracket. Returned
    /// as a `String` so callers can compile once and reuse.
    pub fn capture_regex(self) -> &'static str {
        match self {
            BracketKind::Angle => r"<([^>]*)>",
            BracketKind::Square => r"\[([^\]]*)\]",
            BracketKind::Paren => r"\(([^)]*)\)",
        }
    }
}

/// One pattern reference a stage can take from `book.toml`.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternRef {
    /// Match any of the bracket shapes in order; return the first
    /// hit. The most common shape across the v1 books.
    BracketedTag { brackets: Vec<BracketKind> },

    /// A literal regex. The first capture group, if any, becomes
    /// the inner content; otherwise the whole match.
    Regex(String),
}

/// One pattern match: byte spans into the input plus the inner
/// capture as a freshly-allocated `String`.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternMatch {
    pub start: usize,
    pub end: usize,
    pub inner: String,
}

/// Find the leftmost match of `pattern` in `text`, or `None` if no
/// shape in the reference matches.
///
/// Both branches `expect` the regex compile to succeed:
/// [`BracketKind::capture_regex`] returns a static known-good string,
/// and [`PatternRef::Regex`] is validated by the dispatcher at
/// book.toml load time, so a compile failure here would mean someone
/// constructed an unchecked `PatternRef::Regex` outside the loader.
/// The previous `Regex::new(...).ok()?` silently demoted such a value
/// to "no match", which made stage-level regex bugs invisible.
pub fn match_pattern(pattern: &PatternRef, text: &str) -> Option<PatternMatch> {
    match pattern {
        PatternRef::BracketedTag { brackets } => {
            let mut best: Option<PatternMatch> = None;
            for kind in brackets {
                let re = Regex::new(kind.capture_regex())
                    .expect("BracketKind::capture_regex is a static known-good regex");
                if let Some(cap) = re.captures(text) {
                    let m = cap.get(0)?;
                    let candidate = PatternMatch {
                        start: m.start(),
                        end: m.end(),
                        inner: cap
                            .get(1)
                            .map(|g| g.as_str().to_string())
                            .unwrap_or_default(),
                    };
                    best = match best {
                        Some(b) if b.start <= candidate.start => Some(b),
                        _ => Some(candidate),
                    };
                }
            }
            best
        }
        PatternRef::Regex(src) => {
            let re = Regex::new(src)
                .expect("PatternRef::Regex must be validated by the book.toml loader");
            re.captures(text).and_then(|cap| {
                let m = cap.get(0)?;
                Some(PatternMatch {
                    start: m.start(),
                    end: m.end(),
                    inner: cap
                        .get(1)
                        .map(|g| g.as_str().to_string())
                        .unwrap_or_else(|| m.as_str().to_string()),
                })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_tag_picks_the_first_matching_shape() {
        let pat = PatternRef::BracketedTag {
            brackets: vec![BracketKind::Angle, BracketKind::Square],
        };
        let m = match_pattern(&pat, "abc <American> [extra] def").unwrap();
        assert_eq!(m.inner, "American");
        assert_eq!(&"abc <American> [extra] def"[m.start..m.end], "<American>");
    }

    #[test]
    fn bracketed_tag_returns_the_leftmost_of_alternatives() {
        let pat = PatternRef::BracketedTag {
            brackets: vec![BracketKind::Square, BracketKind::Angle],
        };
        // Even though we listed Square first, the < tag appears
        // earlier; the leftmost match wins regardless of declaration
        // order.
        let m = match_pattern(&pat, "<American> [extra]").unwrap();
        assert_eq!(m.inner, "American");
    }

    #[test]
    fn regex_pattern_uses_the_first_capture_group() {
        let pat = PatternRef::Regex(r"\((\d{4})\)".to_string());
        let m = match_pattern(&pat, "Smith (1900) baseball").unwrap();
        assert_eq!(m.inner, "1900");
    }

    #[test]
    fn no_match_returns_none() {
        let pat = PatternRef::BracketedTag {
            brackets: vec![BracketKind::Angle],
        };
        assert!(match_pattern(&pat, "no tags here").is_none());
    }
}
