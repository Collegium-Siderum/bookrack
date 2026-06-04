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

use bookrack_audit_profile::FilenameParserToggles;

use crate::signals::is_valid_isbn;

/// Fields the filename parser may recover. Every field is `Option` so
/// the caller can merge with extracted values per its own precedence.
/// `author` is parsed because the ingest pipeline writes it to the
/// FRBR-style `node_contributors` table with
/// `origin = "extracted-filename"`, alongside contributors the adapter
/// extracted with `origin = "extracted"`. The flat
/// `node_publication_attrs.author` stopgap column the original v1
/// design contemplated is not needed for that path.
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
/// (all-`None`) value rather than an error. The toggle bag gates the
/// parser as a whole (`enabled`) and the accepted year range
/// (`year_min` / `year_max`).
pub fn parse(stem: &str, toggles: &FilenameParserToggles) -> FilenameBiblio {
    if !toggles.enabled {
        return FilenameBiblio::default();
    }
    let trimmed = stem.trim();
    if trimmed.is_empty() {
        return FilenameBiblio::default();
    }
    if let Some(b) = parse_bracketed_series(trimmed, toggles)
        && !b.is_empty()
    {
        return b;
    }
    if let Some(b) = parse_author_title_paren(trimmed, toggles)
        && !b.is_empty()
    {
        return b;
    }
    if let Some(b) = parse_double_dash(trimmed, toggles)
        && !b.is_empty()
    {
        return b;
    }
    FilenameBiblio::default()
}

/// Template 1: `[Series] <Author - Title (Year, Publisher)>`. The
/// inner part is delegated to template 2; the series segment is
/// attached on success.
fn parse_bracketed_series(stem: &str, toggles: &FilenameParserToggles) -> Option<FilenameBiblio> {
    let rest = stem.strip_prefix('[')?;
    let close = rest.find(']')?;
    let series = rest[..close].trim();
    let tail = rest[close + 1..].trim();
    if series.is_empty() || tail.is_empty() {
        return None;
    }
    let mut inner = parse_author_title_paren(tail, toggles)?;
    inner.series = Some(series.to_string());
    Some(inner)
}

/// Template 2: `Author - Title (Year, Publisher)`. The trailing
/// parenthesis is the anchor: without it the stem is rejected so the
/// double-dash template gets a chance.
fn parse_author_title_paren(stem: &str, toggles: &FilenameParserToggles) -> Option<FilenameBiblio> {
    let without_close = stem.strip_suffix(')')?;
    let open_idx = without_close.rfind('(')?;
    let inside = &without_close[open_idx + 1..];
    let before = without_close[..open_idx].trim_end();

    let (year_part, publisher_part) = inside.split_once(", ")?;
    let year = parse_year(year_part.trim(), toggles)?;
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
fn parse_double_dash(stem: &str, toggles: &FilenameParserToggles) -> Option<FilenameBiblio> {
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
            && let Some(value) = extract_year_anywhere(seg, toggles)
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

/// Accept four ASCII digits in `[year_min, year_max]`; reject anything
/// else. Bounds come from the active filename-parser toggle bag.
fn parse_year<'a>(s: &'a str, toggles: &FilenameParserToggles) -> Option<&'a str> {
    if s.len() != 4 || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = s.parse().ok()?;
    if (toggles.year_min..=toggles.year_max).contains(&n) {
        Some(s)
    } else {
        None
    }
}

/// Scan a segment for any four-digit token that parses as a year.
fn extract_year_anywhere<'a>(seg: &'a str, toggles: &FilenameParserToggles) -> Option<&'a str> {
    seg.split(|c: char| !c.is_ascii_digit())
        .find(|tok| parse_year(tok, toggles).is_some())
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

    fn parse_default(stem: &str) -> FilenameBiblio {
        parse(stem, &FilenameParserToggles::default())
    }

    #[test]
    fn template_simple_paren_picks_apart_author_title_year_publisher() {
        let b = parse_default("Alice Author - A Title (2003, Sample Press)");
        assert_eq!(b.title.as_deref(), Some("A Title"));
        assert_eq!(b.author.as_deref(), Some("Alice Author"));
        assert_eq!(b.year.as_deref(), Some("2003"));
        assert_eq!(b.publisher.as_deref(), Some("Sample Press"));
        assert!(b.series.is_none());
        assert!(b.isbn.is_none());
    }

    #[test]
    fn template_bracketed_series_extracts_series_then_recurses() {
        let b = parse_default("[A Series] Alice Author - A Title (1999, Sample Press)");
        assert_eq!(b.series.as_deref(), Some("A Series"));
        assert_eq!(b.author.as_deref(), Some("Alice Author"));
        assert_eq!(b.title.as_deref(), Some("A Title"));
        assert_eq!(b.year.as_deref(), Some("1999"));
        assert_eq!(b.publisher.as_deref(), Some("Sample Press"));
    }

    #[test]
    fn template_double_dash_recovers_title_author_year_and_isbn() {
        // 9780306406157 is a valid ISBN-13 (transformed from 0-306-40615-2).
        let b = parse_default("A Title -- Alice Author -- 1989 -- isbn13 9780306406157");
        assert_eq!(b.title.as_deref(), Some("A Title"));
        assert_eq!(b.author.as_deref(), Some("Alice Author"));
        assert_eq!(b.year.as_deref(), Some("1989"));
        assert_eq!(b.isbn.as_deref(), Some("9780306406157"));
        assert!(b.publisher.is_none());
    }

    #[test]
    fn double_dash_drops_isbn_that_fails_checksum() {
        // A bare ten-digit timestamp masquerading as ISBN-10.
        let b = parse_default("A Title -- Alice Author -- isbn 1742443234");
        assert!(b.isbn.is_none());
        assert_eq!(b.title.as_deref(), Some("A Title"));
    }

    #[test]
    fn year_must_be_four_digits_in_range() {
        // Year out of accepted range — template 2 rejects, falls through.
        let b = parse_default("Author - Title (1234, Press)");
        assert!(b.is_empty());
        // Non-four-digit year also rejects.
        let b = parse_default("Author - Title (99, Press)");
        assert!(b.is_empty());
    }

    #[test]
    fn empty_stem_returns_default() {
        assert!(parse_default("").is_empty());
        assert!(parse_default("   ").is_empty());
    }

    #[test]
    fn disabled_toggle_returns_empty_biblio() {
        let toggles = FilenameParserToggles {
            enabled: false,
            ..FilenameParserToggles::default()
        };
        let b = parse("Alice Author - A Title (2003, Sample Press)", &toggles);
        assert!(b.is_empty());
    }

    #[test]
    fn year_bounds_from_toggles_are_honoured() {
        // Default `[1500, 2100]` rejects 1234; narrowing to `[1000, 2200]`
        // accepts it.
        let toggles = FilenameParserToggles {
            year_min: 1000,
            year_max: 2200,
            ..FilenameParserToggles::default()
        };
        let b = parse("Author - Title (1234, Press)", &toggles);
        assert_eq!(b.year.as_deref(), Some("1234"));
    }

    #[test]
    fn unmatched_stem_returns_default() {
        // No anchor: no parens, no ` -- `.
        let b = parse_default("just a bare name with no markers");
        assert!(b.is_empty());
    }

    #[test]
    fn isbn10_with_dashes_is_accepted() {
        let b = parse_default("A Title -- Alice -- ISBN: 0-306-40615-2");
        assert_eq!(b.isbn.as_deref(), Some("0306406152"));
    }
}
