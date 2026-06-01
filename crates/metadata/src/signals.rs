// SPDX-License-Identifier: Apache-2.0

//! The audit engine: deterministic per-field grading from the inputs
//! gathered in [`AuditInput`].
//!
//! Each field starts at [`FieldGrade::Strong`] when present and at
//! [`FieldGrade::Missing`] when not. Signals then weaken or strengthen
//! the grade one step at a time, each step appending a [`Flag`] so the
//! report tells a reader why. The aggregate verdict and the row-level
//! confidence are functions of the required-field grades.

use bookrack_extract::TextLayerQuality;

use crate::publishers::{self, PublisherVerdict};
use crate::report::{
    AuditInput, Confidence, FieldGrade, FieldReport, Flag, MetadataReport, Verdict,
};

/// Plausible publication-year range. The lower bound predates movable
/// type but allows for early hand-printed editions; the upper bound is
/// a generous near-future ceiling.
const YEAR_MIN: i32 = 1450;
const YEAR_MAX: i32 = 2100;

/// How many leading blocks of the source are circled as candidate
/// copyright pages.
const COPYRIGHT_CANDIDATE_BLOCKS: usize = 6;

/// Run the audit over the prepared input.
pub(crate) fn run(input: &AuditInput) -> MetadataReport {
    let prior = source_prior(&input.provenance.adapter);
    let doubtful = matches!(
        input.provenance.text_layer_quality,
        TextLayerQuality::Doubtful
    );

    let fields = vec![
        audit_title(input, prior, doubtful),
        audit_language(input, prior, doubtful),
        audit_publisher(input, prior, doubtful),
        audit_year(input, prior, doubtful),
        audit_isbn(input, doubtful),
        audit_subtitle(input, prior, doubtful),
        audit_series(input, prior, doubtful),
    ];

    let verdict = compute_verdict(&fields);
    let confidence = rollup_confidence(&fields);
    let copyright_blocks = (0..input.total_blocks.min(COPYRIGHT_CANDIDATE_BLOCKS)).collect();

    MetadataReport {
        fields,
        verdict,
        confidence,
        copyright_blocks,
    }
}

/// Per-adapter prior on biblio reliability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourcePrior {
    /// Born-digital with structured metadata (EPUB).
    Strong,
    /// Born-digital, weaker biblio (HTML / TXT). PDF lands here too,
    /// since `/Info` is rarely a complete biblio record.
    Weak,
    /// No native biblio surface at all (raw TXT, OCR'd PDF).
    None,
}

fn source_prior(adapter: &str) -> SourcePrior {
    match adapter {
        "epub" | "mobi" | "azw3" => SourcePrior::Strong,
        "pdf" | "html" | "htm" | "xhtml" => SourcePrior::Weak,
        "txt" | "text" | "ocr" => SourcePrior::None,
        _ => SourcePrior::Weak,
    }
}

/// True for adapters whose `/Info` year is more likely a file
/// generation date than a publication year.
fn pdf_year_unreliable(adapter: &str) -> bool {
    matches!(adapter, "pdf")
}

fn audit_title(input: &AuditInput, prior: SourcePrior, doubtful: bool) -> FieldReport {
    let value = input.effective.get("title");
    let mut grade;
    let mut flags = Vec::new();
    match value {
        None => {
            grade = FieldGrade::Missing;
            flags.push(Flag::Empty);
        }
        Some(text) => {
            let trimmed = text.trim();
            grade = FieldGrade::Strong;
            if trimmed.is_empty() {
                grade = FieldGrade::Missing;
                flags.push(Flag::Empty);
            } else {
                if is_placeholder(trimmed) {
                    weaken(&mut grade);
                    flags.push(Flag::PlaceholderValue);
                }
                if trimmed.chars().all(|c| c.is_numeric() || c.is_whitespace()) {
                    weaken(&mut grade);
                    flags.push(Flag::PurelyNumeric);
                }
                if let Some(stem) = input.source_stem
                    && stem.eq_ignore_ascii_case(trimmed)
                {
                    weaken(&mut grade);
                    flags.push(Flag::EqualsFilename);
                }
                if let Some(pub_value) = input.effective.get("publisher")
                    && pub_value.eq_ignore_ascii_case(trimmed)
                {
                    weaken(&mut grade);
                    flags.push(Flag::EqualsPublisher);
                }
            }
        }
    }
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some());
    apply_doubtful(&mut grade, &mut flags, doubtful, value.is_some());

    let hint = title_hint(&flags, grade);
    FieldReport {
        field: "title".to_string(),
        grade,
        flags,
        hint,
    }
}

fn audit_language(input: &AuditInput, prior: SourcePrior, doubtful: bool) -> FieldReport {
    let value = input.effective.get("language");
    let mut grade;
    let mut flags = Vec::new();
    match value {
        None => {
            grade = FieldGrade::Missing;
            flags.push(Flag::Empty);
        }
        Some(text) => {
            let trimmed = text.trim();
            grade = FieldGrade::Strong;
            if trimmed.is_empty() {
                grade = FieldGrade::Missing;
                flags.push(Flag::Empty);
            } else {
                if !is_bcp47(trimmed) {
                    weaken(&mut grade);
                    flags.push(Flag::NonBcp47);
                }
                if !body_matches_language(trimmed, input.body_sample) {
                    weaken(&mut grade);
                    flags.push(Flag::LangMismatchesBody);
                }
            }
        }
    }
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some());
    apply_doubtful(&mut grade, &mut flags, doubtful, value.is_some());

    let hint = if flags.is_empty() {
        "language present and matches body sample".to_string()
    } else if flags.contains(&Flag::Empty) {
        "language is missing; set from the body's script".to_string()
    } else {
        "language present but flagged; review against body sample".to_string()
    };
    FieldReport {
        field: "language".to_string(),
        grade,
        flags,
        hint,
    }
}

fn audit_publisher(input: &AuditInput, prior: SourcePrior, doubtful: bool) -> FieldReport {
    let value = input.effective.get("publisher");
    let mut grade;
    let mut flags = Vec::new();
    match value {
        None => {
            grade = FieldGrade::Missing;
            flags.push(Flag::Empty);
        }
        Some(text) => {
            let trimmed = text.trim();
            grade = FieldGrade::Strong;
            if trimmed.is_empty() {
                grade = FieldGrade::Missing;
                flags.push(Flag::Empty);
            } else {
                match publishers::evaluate(trimmed) {
                    PublisherVerdict::Watermark => {
                        // Two notches: watermark is structurally not a
                        // publisher.
                        weaken(&mut grade);
                        weaken(&mut grade);
                        flags.push(Flag::SourceWatermark);
                    }
                    PublisherVerdict::Whitelisted => {
                        flags.push(Flag::PublisherWhitelisted);
                    }
                    PublisherVerdict::Neutral => {}
                }
            }
        }
    }
    // Apply source prior before the whitelist strengthens the grade,
    // so the whitelist match can offset a weak prior.
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some());
    apply_doubtful(&mut grade, &mut flags, doubtful, value.is_some());
    if flags.contains(&Flag::PublisherWhitelisted) {
        strengthen(&mut grade);
    }

    let hint = if flags.contains(&Flag::SourceWatermark) {
        "publisher value looks like a distribution watermark".to_string()
    } else if flags.contains(&Flag::Empty) {
        "publisher missing".to_string()
    } else if flags.contains(&Flag::PublisherWhitelisted) {
        "publisher matched the reputable-imprint list".to_string()
    } else {
        "publisher present but not corroborated".to_string()
    };
    FieldReport {
        field: "publisher".to_string(),
        grade,
        flags,
        hint,
    }
}

fn audit_year(input: &AuditInput, prior: SourcePrior, doubtful: bool) -> FieldReport {
    let value = input.effective.get("year");
    let mut grade;
    let mut flags = Vec::new();
    match value {
        None => {
            grade = FieldGrade::Missing;
            flags.push(Flag::Empty);
        }
        Some(text) => {
            grade = FieldGrade::Strong;
            match text.trim().parse::<i32>() {
                Err(_) => {
                    weaken(&mut grade);
                    flags.push(Flag::YearOutOfRange);
                }
                Ok(year) => {
                    if !(YEAR_MIN..=YEAR_MAX).contains(&year) {
                        weaken(&mut grade);
                        flags.push(Flag::YearOutOfRange);
                    }
                }
            }
            if pdf_year_unreliable(&input.provenance.adapter) {
                weaken(&mut grade);
                flags.push(Flag::PdfYearLikelyFileDate);
            }
        }
    }
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some());
    apply_doubtful(&mut grade, &mut flags, doubtful, value.is_some());

    let hint = if flags.contains(&Flag::YearOutOfRange) {
        "publication year falls outside the plausible range".to_string()
    } else if flags.contains(&Flag::PdfYearLikelyFileDate) {
        "PDF year is often the file creation date, not the work year".to_string()
    } else if flags.contains(&Flag::Empty) {
        "publication year missing".to_string()
    } else {
        "publication year plausible".to_string()
    };
    FieldReport {
        field: "year".to_string(),
        grade,
        flags,
        hint,
    }
}

fn audit_isbn(input: &AuditInput, doubtful: bool) -> FieldReport {
    let value = input.effective.get("isbn");
    let mut grade;
    let mut flags = Vec::new();
    match value {
        None => {
            // ISBN is an optional field; a missing one stays neutral.
            grade = FieldGrade::Medium;
        }
        Some(text) => {
            grade = FieldGrade::Strong;
            if !is_valid_isbn(text.trim()) {
                weaken(&mut grade);
                flags.push(Flag::IsbnCheckFailed);
            }
        }
    }
    apply_doubtful(&mut grade, &mut flags, doubtful, value.is_some());

    let hint = if flags.contains(&Flag::IsbnCheckFailed) {
        "ISBN checksum did not validate".to_string()
    } else if value.is_none() {
        "no ISBN recorded (optional)".to_string()
    } else {
        "ISBN checksum valid".to_string()
    };
    FieldReport {
        field: "isbn".to_string(),
        grade,
        flags,
        hint,
    }
}

fn audit_subtitle(input: &AuditInput, prior: SourcePrior, doubtful: bool) -> FieldReport {
    audit_optional(input, prior, doubtful, "subtitle")
}

fn audit_series(input: &AuditInput, prior: SourcePrior, doubtful: bool) -> FieldReport {
    audit_optional(input, prior, doubtful, "series")
}

fn audit_optional(
    input: &AuditInput,
    prior: SourcePrior,
    doubtful: bool,
    field: &str,
) -> FieldReport {
    let value = input.effective.get(field);
    let mut grade;
    let mut flags = Vec::new();
    match value {
        None => {
            grade = FieldGrade::Medium;
        }
        Some(text) => {
            grade = FieldGrade::Strong;
            if text.trim().is_empty() {
                grade = FieldGrade::Medium;
                flags.push(Flag::Empty);
            }
        }
    }
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some());
    apply_doubtful(&mut grade, &mut flags, doubtful, value.is_some());
    FieldReport {
        field: field.to_string(),
        grade,
        flags,
        hint: format!("{field} (optional)"),
    }
}

/// Weaken a grade by one notch. [`FieldGrade::Missing`] is the floor;
/// further weakenings are silent.
fn weaken(grade: &mut FieldGrade) {
    *grade = match *grade {
        FieldGrade::Strong => FieldGrade::Medium,
        FieldGrade::Medium => FieldGrade::Weak,
        FieldGrade::Weak => FieldGrade::Missing,
        FieldGrade::Missing => FieldGrade::Missing,
    };
}

/// Strengthen a grade by one notch. [`FieldGrade::Strong`] is the
/// ceiling; further strengthenings are silent.
fn strengthen(grade: &mut FieldGrade) {
    *grade = match *grade {
        FieldGrade::Missing => FieldGrade::Weak,
        FieldGrade::Weak => FieldGrade::Medium,
        FieldGrade::Medium => FieldGrade::Strong,
        FieldGrade::Strong => FieldGrade::Strong,
    };
}

fn apply_source_prior(
    grade: &mut FieldGrade,
    flags: &mut Vec<Flag>,
    prior: SourcePrior,
    present: bool,
) {
    if !present {
        return;
    }
    match prior {
        SourcePrior::Strong => {}
        SourcePrior::Weak | SourcePrior::None => {
            weaken(grade);
            flags.push(Flag::SourcePriorWeak);
        }
    }
}

fn apply_doubtful(grade: &mut FieldGrade, flags: &mut Vec<Flag>, doubtful: bool, present: bool) {
    if !doubtful || !present {
        return;
    }
    weaken(grade);
    flags.push(Flag::DoubtfulTextLayer);
}

fn compute_verdict(fields: &[FieldReport]) -> Verdict {
    for field in fields {
        if !matches!(field.field.as_str(), "title" | "language") {
            continue;
        }
        if matches!(field.grade, FieldGrade::Missing | FieldGrade::Weak) {
            return Verdict::NeedsWork;
        }
    }
    Verdict::Clean
}

fn rollup_confidence(fields: &[FieldReport]) -> Confidence {
    let required_or_should = ["title", "language", "publisher", "year"];
    let mut any_missing = false;
    let mut any_weak = false;
    let mut all_strong = true;
    for field in fields {
        if !required_or_should.contains(&field.field.as_str()) {
            continue;
        }
        match field.grade {
            FieldGrade::Missing => {
                any_missing = true;
                all_strong = false;
            }
            FieldGrade::Weak => {
                any_weak = true;
                all_strong = false;
            }
            FieldGrade::Medium => {
                all_strong = false;
            }
            FieldGrade::Strong => {}
        }
    }
    if any_missing {
        Confidence::Low
    } else if all_strong {
        Confidence::High
    } else if any_weak {
        Confidence::Low
    } else {
        Confidence::Medium
    }
}

fn title_hint(flags: &[Flag], grade: FieldGrade) -> String {
    if flags.contains(&Flag::Empty) {
        return "title missing".to_string();
    }
    if flags.contains(&Flag::EqualsFilename) {
        return "title equals the source filename".to_string();
    }
    if flags.contains(&Flag::PlaceholderValue) {
        return "title is a placeholder value".to_string();
    }
    if grade == FieldGrade::Strong {
        "title present and clean".to_string()
    } else {
        "title present but flagged".to_string()
    }
}

fn is_placeholder(value: &str) -> bool {
    let lower = value.to_lowercase();
    let stripped = lower.trim_matches(|c: char| !c.is_alphanumeric());
    matches!(
        stripped,
        "unknown" | "untitled" | "no title" | "anonymous" | "n a" | "na"
    )
}

/// Light BCP-47 syntactic check: a primary subtag plus an optional
/// region or script subtag. Not a full registry validation.
fn is_bcp47(tag: &str) -> bool {
    let mut iter = tag.split('-');
    let Some(primary) = iter.next() else {
        return false;
    };
    if !(2..=3).contains(&primary.len()) || !primary.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for sub in iter {
        let len = sub.len();
        let ok = match len {
            2 => sub.chars().all(|c| c.is_ascii_alphabetic()),
            3 => sub.chars().all(|c| c.is_ascii_alphanumeric()),
            4 => sub.chars().all(|c| c.is_ascii_alphabetic()),
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Compare a declared language against the body sample's script.
///
/// Conservative: only fires when the tag and the body strongly
/// disagree. Returns true when nothing in the body sample
/// contradicts the declaration.
fn body_matches_language(tag: &str, body: &str) -> bool {
    if body.is_empty() {
        return true;
    }
    let counted: Vec<char> = body
        .chars()
        .filter(|c| c.is_alphabetic() || is_cjk(*c))
        .take(2048)
        .collect();
    if counted.is_empty() {
        return true;
    }
    let cjk = counted.iter().filter(|c| is_cjk(**c)).count();
    let latin = counted.iter().filter(|c| c.is_ascii_alphabetic()).count();
    let total = counted.len();
    let cjk_ratio = cjk as f64 / total as f64;
    let latin_ratio = latin as f64 / total as f64;

    let primary = tag.split('-').next().unwrap_or("").to_ascii_lowercase();
    let declared_cjk = matches!(primary.as_str(), "zh" | "ja" | "ko");
    let declared_latin = matches!(
        primary.as_str(),
        "en" | "fr" | "de" | "es" | "it" | "pt" | "nl" | "sv" | "no" | "da" | "fi" | "pl" | "ru"
    );

    if declared_cjk && cjk_ratio < 0.1 && latin_ratio > 0.6 {
        return false;
    }
    if declared_latin && cjk_ratio > 0.4 {
        return false;
    }
    true
}

/// A CJK ideograph (the common ranges). Kept private here to avoid
/// pulling extract's internal `quality` module into the public surface.
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
        | 0x2_0000..=0x2_A6DF | 0x2_A700..=0x2_EBEF)
}

/// Validate an ISBN-10 or ISBN-13 by checksum. Hyphens and spaces are
/// stripped first; any other character invalidates the value.
fn is_valid_isbn(value: &str) -> bool {
    let digits: Vec<char> = value
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    match digits.len() {
        10 => is_valid_isbn10(&digits),
        13 => is_valid_isbn13(&digits),
        _ => false,
    }
}

fn is_valid_isbn10(digits: &[char]) -> bool {
    let mut sum = 0i32;
    for (i, ch) in digits.iter().enumerate() {
        let value = if i == 9 && (*ch == 'X' || *ch == 'x') {
            10
        } else if let Some(d) = ch.to_digit(10) {
            d as i32
        } else {
            return false;
        };
        sum += value * (10 - i as i32);
    }
    sum % 11 == 0
}

fn is_valid_isbn13(digits: &[char]) -> bool {
    let mut sum = 0i32;
    for (i, ch) in digits.iter().enumerate() {
        let Some(d) = ch.to_digit(10) else {
            return false;
        };
        let weight = if i % 2 == 0 { 1 } else { 3 };
        sum += d as i32 * weight;
    }
    sum % 10 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isbn10_validator_accepts_known_good() {
        assert!(is_valid_isbn("0-306-40615-2"));
        assert!(is_valid_isbn("030640615 2"));
    }

    #[test]
    fn isbn13_validator_accepts_known_good() {
        assert!(is_valid_isbn("978-3-16-148410-0"));
    }

    #[test]
    fn isbn_validator_rejects_bad_checksum() {
        assert!(!is_valid_isbn("0-306-40615-3"));
        assert!(!is_valid_isbn("978-3-16-148410-1"));
    }

    #[test]
    fn bcp47_accepts_common_tags() {
        assert!(is_bcp47("en"));
        assert!(is_bcp47("en-US"));
        assert!(is_bcp47("zh-Hans"));
        assert!(is_bcp47("zh-CN"));
    }

    #[test]
    fn bcp47_rejects_garbage() {
        assert!(!is_bcp47(""));
        assert!(!is_bcp47("english"));
        assert!(!is_bcp47("z"));
        assert!(!is_bcp47("zh--"));
    }
}
