// SPDX-License-Identifier: Apache-2.0

//! Pattern references used by the splitter and extractor stages.
//!
//! [`PatternRef`] is what `book.toml` passes to stages like
//! `partition_body_around_match` and `extract_bracketed_tag`: either
//! one of a small set of bracketed-tag shapes or an explicit regex.
//! Both shapes carry an already-compiled regex — the bracket shapes
//! from process-wide statics, the explicit regex from the `book.toml`
//! loader — so [`match_pattern`] only matches, however many entries a
//! stage runs over. It returns the matched span plus the inner
//! capture, so stages can both write the captured value into a
//! payload key and strip the match from the source text.

use std::sync::LazyLock;

use regex::Regex;

/// Opening and closing angle-bracket shapes, in matching order:
/// ASCII, CJK (U+3008/3009), the deprecated CJK-compatibility pair
/// that normalizes onto it (U+2329/232A), and the full-width
/// less-than / greater-than signs. The double-width pair
/// (U+300A/300B) is deliberately absent: it wraps work titles rather
/// than tags.
const ANGLE_OPEN: &str = "<\u{3008}\u{2329}\u{FF1C}";
const ANGLE_CLOSE: &str = ">\u{3009}\u{232A}\u{FF1E}";

static ANGLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("[{ANGLE_OPEN}]([^{ANGLE_CLOSE}]*)[{ANGLE_CLOSE}]")).expect("angle regex")
});
static SQUARE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]").expect("square regex"));
static PAREN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(([^)]*)\)").expect("paren regex"));

/// The two bracket-shaped tag patterns books actually use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BracketKind {
    /// Any of the angle-bracket shapes, ASCII or CJK. The opening and
    /// closing shapes are matched as two character classes rather than
    /// as fixed pairs, so a tag whose OCR recovered only one side in
    /// its original width still matches.
    Angle,
    Square,
    Paren,
}

impl BracketKind {
    /// Regex capturing the inner contents of the bracket, compiled
    /// once per process and shared by every stage run.
    pub fn regex(self) -> &'static Regex {
        match self {
            BracketKind::Angle => &ANGLE_RE,
            BracketKind::Square => &SQUARE_RE,
            BracketKind::Paren => &PAREN_RE,
        }
    }

    /// Source text of [`Self::regex`].
    pub fn capture_regex(self) -> &'static str {
        self.regex().as_str()
    }
}

/// One pattern reference a stage can take from `book.toml`.
#[derive(Debug, Clone)]
pub enum PatternRef {
    /// Match any of the bracket shapes in order; return the first
    /// hit. The most common shape across the v1 books.
    BracketedTag { brackets: Vec<BracketKind> },

    /// A literal regex, compiled by the `book.toml` loader. The first
    /// capture group, if any, becomes the inner content; otherwise
    /// the whole match.
    Regex(Regex),
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
/// Matching only: both branches read a regex that was compiled before
/// the stage ran, so a `book.toml` pattern that does not compile is
/// rejected at load time with [`ParseError::InvalidPattern`] rather
/// than reaching this function.
///
/// [`ParseError::InvalidPattern`]: crate::error::ParseError::InvalidPattern
pub fn match_pattern(pattern: &PatternRef, text: &str) -> Option<PatternMatch> {
    match_pattern_skipping(pattern, text, &[])
}

/// [`match_pattern`], ignoring any match whose inner capture equals one
/// of `skip_inner`.
///
/// A book can spell two kinds of tag with one bracket shape — a country
/// marker and a name-type marker both in angle brackets. Naming the
/// values that are not the wanted tag lets the leftmost-match rule walk
/// past them; with every candidate skipped the result is `None`, the
/// same as no match at all.
pub fn match_pattern_skipping(
    pattern: &PatternRef,
    text: &str,
    skip_inner: &[String],
) -> Option<PatternMatch> {
    let wanted = |cap: &regex::Captures<'_>, whole_on_empty: bool| -> Option<PatternMatch> {
        let m = cap.get(0)?;
        let inner = match cap.get(1) {
            Some(g) => g.as_str().to_string(),
            None if whole_on_empty => m.as_str().to_string(),
            None => String::new(),
        };
        if skip_inner.contains(&inner) {
            return None;
        }
        Some(PatternMatch {
            start: m.start(),
            end: m.end(),
            inner,
        })
    };

    match pattern {
        PatternRef::BracketedTag { brackets } => {
            let mut best: Option<PatternMatch> = None;
            for kind in brackets {
                // Each shape contributes its own leftmost non-skipped
                // match; the leftmost across shapes wins, as it does
                // without a skip list.
                let Some(candidate) = kind
                    .regex()
                    .captures_iter(text)
                    .find_map(|cap| wanted(&cap, false))
                else {
                    continue;
                };
                best = match best {
                    Some(b) if b.start <= candidate.start => Some(b),
                    _ => Some(candidate),
                };
            }
            best
        }
        PatternRef::Regex(re) => re.captures_iter(text).find_map(|cap| wanted(&cap, true)),
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
    fn angle_matches_the_cjk_and_full_width_bracket_shapes() {
        let pat = PatternRef::BracketedTag {
            brackets: vec![BracketKind::Angle],
        };
        // U+3008/3009, U+2329/232A and U+FF1C/FF1E all carry the same
        // tag shape as the ASCII pair in OCR text.
        for text in [
            "\u{963F}\u{683C}\u{7279}\u{3008}\u{82F1}\u{3009}",
            "\u{963F}\u{683C}\u{7279}\u{2329}\u{82F1}\u{232A}",
            "\u{963F}\u{683C}\u{7279}\u{FF1C}\u{82F1}\u{FF1E}",
        ] {
            let m =
                match_pattern(&pat, text).unwrap_or_else(|| panic!("no angle match in {text:?}"));
            assert_eq!(m.inner, "\u{82F1}", "wrong capture in {text:?}");
            assert_eq!(&text[m.start..m.end], &text[9..]);
        }
    }

    #[test]
    fn angle_leaves_double_angle_brackets_alone() {
        // U+300A/300B wrap work titles, not tags. Folding them into the
        // angle shape would write a book title into the tag's payload
        // key and cut it out of the surrounding prose.
        let pat = PatternRef::BracketedTag {
            brackets: vec![BracketKind::Angle],
        };
        assert!(match_pattern(&pat, "\u{300A}\u{5723}\u{7ECF}\u{300B}").is_none());
    }

    #[test]
    fn regex_pattern_uses_the_first_capture_group() {
        let pat = PatternRef::Regex(Regex::new(r"\((\d{4})\)").unwrap());
        let m = match_pattern(&pat, "Smith (1900) baseball").unwrap();
        assert_eq!(m.inner, "1900");
    }

    #[test]
    fn regex_pattern_without_a_group_captures_the_whole_match() {
        let pat = PatternRef::Regex(Regex::new(r"\d{4}").unwrap());
        let m = match_pattern(&pat, "Smith 1900 baseball").unwrap();
        assert_eq!(m.inner, "1900");
        assert_eq!(&"Smith 1900 baseball"[m.start..m.end], "1900");
    }

    #[test]
    fn bracket_kinds_hand_out_one_shared_compilation() {
        // `regex()` returns a borrow of the process-wide static, so
        // repeated calls must land on the same allocation rather than
        // compiling per call.
        assert!(std::ptr::eq(
            BracketKind::Angle.regex(),
            BracketKind::Angle.regex()
        ));
        assert_eq!(BracketKind::Square.capture_regex(), r"\[([^\]]*)\]");
    }

    #[test]
    fn no_match_returns_none() {
        let pat = PatternRef::BracketedTag {
            brackets: vec![BracketKind::Angle],
        };
        assert!(match_pattern(&pat, "no tags here").is_none());
    }
}
