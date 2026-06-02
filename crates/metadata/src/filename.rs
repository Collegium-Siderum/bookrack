// SPDX-License-Identifier: Apache-2.0

//! Filename → tentative biblio parser.
//!
//! Pure function: hand a file stem in, get back a [`FilenameBiblio`]
//! filled with whatever the three supported templates could recognize.
//! The caller decides how to merge those tentative values with the
//! adapter-extracted biblio — see `bookrack-ingest` for the merge.
//!
//! Three Calibre-flavoured templates are tried in priority order; the
//! first whose anchor matches wins:
//!
//! 1. `[Series] Author - Title (Year, Publisher)` — bracketed-series
//!    prefix, otherwise the same shape as template 2.
//! 2. `Author - Title (Year, Publisher)` — the most common
//!    Calibre-export form for PDF / EPUB.
//! 3. `Title -- Author -- ... -- isbn13 NNNN` — the long-dash chain
//!    Calibre-import emits for TXT / EPUB rescues.
//!
//! Validators are strict on the parts that downstream audit relies on:
//! `year` must be four ASCII digits in `1500..=2100`; `isbn` must pass
//! the ISBN-10 / ISBN-13 checksum so that a Unix-timestamp masquerading
//! as an identifier never reaches base attrs.

use crate::signals::is_valid_isbn;

/// Fields the filename parser may recover. Every field is `Option` so
/// the caller can merge with extracted values per its own precedence.
/// `author` is parsed even though `node_publication_attrs` has no
/// author column today; future schema additions consume it without
/// re-running the parser.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilenameBiblio {
    pub title: Option<String>,
    pub author: Option<String>,
    pub year: Option<String>,
    pub publisher: Option<String>,
    pub isbn: Option<String>,
    pub series: Option<String>,
}

impl FilenameBiblio {
    /// True when no field was recovered.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.author.is_none()
            && self.year.is_none()
            && self.publisher.is_none()
            && self.isbn.is_none()
            && self.series.is_none()
    }
}

/// Try each template against `stem`, returning the first match. An
/// empty input or one that matches no template returns the default
/// (all-`None`) value rather than an error.
pub fn parse(stem: &str) -> FilenameBiblio {
    let trimmed = stem.trim();
    if trimmed.is_empty() {
        return FilenameBiblio::default();
    }
    if let Some(b) = parse_bracketed_series(trimmed)
        && !b.is_empty()
    {
        return b;
    }
    if let Some(b) = parse_author_title_paren(trimmed)
        && !b.is_empty()
    {
        return b;
    }
    if let Some(b) = parse_double_dash(trimmed)
        && !b.is_empty()
    {
        return b;
    }
    FilenameBiblio::default()
}

/// Template 1: `[Series] <Author - Title (Year, Publisher)>`. The
/// inner part is delegated to template 2; the series segment is
/// attached on success.
fn parse_bracketed_series(stem: &str) -> Option<FilenameBiblio> {
    let rest = stem.strip_prefix('[')?;
    let close = rest.find(']')?;
    let series = rest[..close].trim();
    let tail = rest[close + 1..].trim();
    if series.is_empty() || tail.is_empty() {
        return None;
    }
    let mut inner = parse_author_title_paren(tail)?;
    inner.series = Some(series.to_string());
    Some(inner)
}

/// Template 2: `Author - Title (Year, Publisher)`. The trailing
/// parenthesis is the anchor: without it the stem is rejected so the
/// double-dash template gets a chance.
fn parse_author_title_paren(stem: &str) -> Option<FilenameBiblio> {
    let without_close = stem.strip_suffix(')')?;
    let open_idx = without_close.rfind('(')?;
    let inside = &without_close[open_idx + 1..];
    let before = without_close[..open_idx].trim_end();

    let (year_part, publisher_part) = inside.split_once(", ")?;
    let year = parse_year(year_part.trim())?;
    let publisher = publisher_part.trim();
    if publisher.is_empty() {
        return None;
    }

    let (author, title) = before.split_once(" - ")?;
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    Some(FilenameBiblio {
        title: Some(title.to_string()),
        author: nonempty(author.trim()),
        year: Some(year.to_string()),
        publisher: Some(publisher.to_string()),
        isbn: None,
        series: None,
    })
}

/// Template 3: `Title -- Author -- ... -- isbn13 NNNN`. Splits on the
/// double-dash separator; the first segment is taken as the title,
/// the second (if non-empty) as the author, and later segments are
/// scanned for an ISBN payload. Year inside a trailing parenthesis on
/// any segment is captured opportunistically.
fn parse_double_dash(stem: &str) -> Option<FilenameBiblio> {
    if !stem.contains(" -- ") {
        return None;
    }
    let parts: Vec<&str> = stem.split(" -- ").map(str::trim).collect();
    if parts.len() < 2 {
        return None;
    }

    let title = nonempty(parts[0]);
    let author = parts.get(1).copied().and_then(nonempty);

    let mut isbn = None;
    let mut year = None;
    for seg in parts.iter().skip(1) {
        if isbn.is_none()
            && let Some(value) = extract_isbn(seg)
        {
            isbn = Some(value);
        }
        if year.is_none()
            && let Some(value) = extract_year_anywhere(seg)
        {
            year = Some(value.to_string());
        }
    }

    if title.is_none() && author.is_none() && isbn.is_none() && year.is_none() {
        return None;
    }
    Some(FilenameBiblio {
        title,
        author,
        year,
        publisher: None,
        isbn,
        series: None,
    })
}

/// Accept four ASCII digits in `1500..=2100`; reject anything else.
fn parse_year(s: &str) -> Option<&str> {
    if s.len() != 4 || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = s.parse().ok()?;
    if (1500..=2100).contains(&n) {
        Some(s)
    } else {
        None
    }
}

/// Scan a segment for any four-digit token that parses as a year.
fn extract_year_anywhere(seg: &str) -> Option<&str> {
    seg.split(|c: char| !c.is_ascii_digit())
        .find(|tok| parse_year(tok).is_some())
}

/// Pull an ISBN out of a segment. Recognised prefixes are `isbn13`,
/// `isbn10`, and `isbn` (case-insensitive), each optionally followed
/// by `:`, `_`, or whitespace. Only digits and a final `X` survive
/// into the candidate; the result is gated by [`is_valid_isbn`] so a
/// bare timestamp or arbitrary numeric tail is rejected.
fn extract_isbn(seg: &str) -> Option<String> {
    let lower = seg.to_ascii_lowercase();
    let after_label = ["isbn13", "isbn10", "isbn"]
        .iter()
        .find_map(|label| lower.strip_prefix(label))
        .unwrap_or(&lower);
    let after_sep = after_label.trim_start_matches([' ', ':', '_', '-']);
    let candidate: String = after_sep
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '-' | ' ' | 'x' | 'X' | '_'))
        .filter(|c| c.is_ascii_digit() || matches!(c, 'x' | 'X'))
        .collect();
    if is_valid_isbn(&candidate) {
        Some(candidate.to_uppercase())
    } else {
        None
    }
}

fn nonempty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_simple_paren_picks_apart_author_title_year_publisher() {
        let b = parse("Alice Author - A Title (2003, Sample Press)");
        assert_eq!(b.title.as_deref(), Some("A Title"));
        assert_eq!(b.author.as_deref(), Some("Alice Author"));
        assert_eq!(b.year.as_deref(), Some("2003"));
        assert_eq!(b.publisher.as_deref(), Some("Sample Press"));
        assert!(b.series.is_none());
        assert!(b.isbn.is_none());
    }

    #[test]
    fn template_bracketed_series_extracts_series_then_recurses() {
        let b = parse("[A Series] Alice Author - A Title (1999, Sample Press)");
        assert_eq!(b.series.as_deref(), Some("A Series"));
        assert_eq!(b.author.as_deref(), Some("Alice Author"));
        assert_eq!(b.title.as_deref(), Some("A Title"));
        assert_eq!(b.year.as_deref(), Some("1999"));
        assert_eq!(b.publisher.as_deref(), Some("Sample Press"));
    }

    #[test]
    fn template_double_dash_recovers_title_author_year_and_isbn() {
        // 9780306406157 is a valid ISBN-13 (transformed from 0-306-40615-2).
        let b = parse("A Title -- Alice Author -- 1989 -- isbn13 9780306406157");
        assert_eq!(b.title.as_deref(), Some("A Title"));
        assert_eq!(b.author.as_deref(), Some("Alice Author"));
        assert_eq!(b.year.as_deref(), Some("1989"));
        assert_eq!(b.isbn.as_deref(), Some("9780306406157"));
        assert!(b.publisher.is_none());
    }

    #[test]
    fn double_dash_drops_isbn_that_fails_checksum() {
        // A bare ten-digit timestamp masquerading as ISBN-10.
        let b = parse("A Title -- Alice Author -- isbn 1742443234");
        assert!(b.isbn.is_none());
        assert_eq!(b.title.as_deref(), Some("A Title"));
    }

    #[test]
    fn year_must_be_four_digits_in_range() {
        // Year out of accepted range — template 2 rejects, falls through.
        let b = parse("Author - Title (1234, Press)");
        assert!(b.is_empty());
        // Non-four-digit year also rejects.
        let b = parse("Author - Title (99, Press)");
        assert!(b.is_empty());
    }

    #[test]
    fn empty_stem_returns_default() {
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
    }

    #[test]
    fn unmatched_stem_returns_default() {
        // No anchor: no parens, no ` -- `.
        let b = parse("just a bare name with no markers");
        assert!(b.is_empty());
    }

    #[test]
    fn isbn10_with_dashes_is_accepted() {
        let b = parse("A Title -- Alice -- ISBN: 0-306-40615-2");
        assert_eq!(b.isbn.as_deref(), Some("0306406152"));
    }
}
