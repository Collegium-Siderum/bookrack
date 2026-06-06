// SPDX-License-Identifier: Apache-2.0

//! The audit engine: deterministic per-field grading from the inputs
//! gathered in [`AuditInput`].
//!
//! Each field starts at [`FieldGrade::Strong`] when present and at
//! [`FieldGrade::Missing`] when not. Signals then weaken or strengthen
//! the grade one step at a time, each step appending a [`Flag`] so the
//! report tells a reader why. The aggregate verdict and the row-level
//! confidence are functions of the required-field grades.
//!
//! Every weakening signal is gated by a corresponding toggle in
//! [`bookrack_audit_profile::AuditProfile`]. A signal whose toggle is
//! off neither weakens the grade nor appends its flag, so the audit
//! reduces to plain "present / missing" reporting under the
//! `trust-source` profile.

use bookrack_audit_profile::AuditProfile;
use bookrack_extract::TextLayerQuality;

use crate::publishers::{self, PublisherVerdict};
use crate::report::{
    AuditInput, Confidence, FieldGrade, FieldReport, Flag, MetadataReport, Verdict,
};

/// How a TOC's shape audit lands on the verdict / confidence dial.
///
/// The shape signals are a separate channel from the per-field grades:
/// they can only push the verdict toward [`Verdict::NeedsWork`] and the
/// confidence toward [`Confidence::Low`], never the other direction.
/// This invariant is enforced by [`apply_shape_to_verdict_and_confidence`]
/// and covered by a 32-combination table-driven test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeSeverity {
    /// No shape flags fired; verdict and confidence stay where the
    /// per-field rollup left them.
    Clean,
    /// At least one shape flag fired but none crossed the severe
    /// threshold; confidence is capped at [`Confidence::Medium`].
    Mild,
    /// A strong shape signal fired; verdict is forced to
    /// [`Verdict::NeedsWork`] and confidence to [`Confidence::Low`].
    Severe,
}

/// Run the audit over the prepared input under one profile.
pub(crate) fn run(input: &AuditInput, profile: &AuditProfile) -> MetadataReport {
    let prior = source_prior(&input.provenance.adapter);
    let doubtful = matches!(
        input.provenance.text_layer_quality,
        TextLayerQuality::Doubtful
    );

    let fields = vec![
        audit_title(input, profile, prior, doubtful),
        audit_language(input, profile, prior, doubtful),
        audit_publisher(input, profile, prior, doubtful),
        audit_year(input, profile, prior, doubtful),
        audit_isbn(input, doubtful),
        audit_subtitle(input, profile, prior, doubtful),
        audit_series(input, profile, prior, doubtful),
    ];

    let mut verdict = compute_verdict(&fields);
    let mut confidence = rollup_confidence(&fields);
    let shape_flags = audit_toc_shape(input, profile);
    let severity = shape_severity(&shape_flags, input.toc_stats.total_toc_entries, profile);
    apply_shape_to_verdict_and_confidence(&mut verdict, &mut confidence, severity);
    let copyright_blocks = if profile.copyright_blocks.enabled {
        (0..input.total_blocks.min(profile.copyright_blocks.count)).collect()
    } else {
        Vec::new()
    };

    MetadataReport {
        fields,
        verdict,
        confidence,
        copyright_blocks,
        shape_flags,
    }
}

/// Audit the TOC shape. Emits at most five flags in a fixed order so
/// the resulting `Vec<Flag>` is byte-stable across runs over the same
/// [`TocStats`]:
///
/// 1. [`Flag::TocEmptyLargeBody`]
/// 2. [`Flag::TocUnanchoredSome`]
/// 3. [`Flag::TocUnanchoredHalf`]
/// 4. [`Flag::TocSuspiciousFlat`]
/// 5. [`Flag::TocHeadingBlockSkew`]
pub(crate) fn audit_toc_shape(input: &AuditInput, profile: &AuditProfile) -> Vec<Flag> {
    let stats = input.toc_stats;
    let shape = &profile.toc_shape;
    let mut flags = Vec::new();
    if shape.empty_large_body
        && stats.total_toc_entries == 0
        && input.total_blocks > shape.large_body_min_blocks
    {
        flags.push(Flag::TocEmptyLargeBody);
    }
    if stats.unanchored_toc_entries > 0 {
        flags.push(Flag::TocUnanchoredSome);
    }
    if stats.total_toc_entries > 0
        && stats
            .unanchored_toc_entries
            .saturating_mul(2)
            .gt(&stats.total_toc_entries)
    {
        flags.push(Flag::TocUnanchoredHalf);
    }
    if shape.suspicious_flat && stats.suspicious_flat {
        flags.push(Flag::TocSuspiciousFlat);
    }
    if shape.heading_block_skew && stats.heading_block_skew {
        flags.push(Flag::TocHeadingBlockSkew);
    }
    flags
}

/// Classify a TOC shape into a severity band. Severe outranks mild
/// outranks clean.
pub(crate) fn shape_severity(
    flags: &[Flag],
    total_entries: usize,
    profile: &AuditProfile,
) -> ShapeSeverity {
    let shape = &profile.toc_shape;
    let severe = flags.contains(&Flag::TocUnanchoredHalf)
        || flags.contains(&Flag::TocEmptyLargeBody)
        || (flags.contains(&Flag::TocSuspiciousFlat)
            && total_entries > shape.flat_severe_min_entries)
        || (flags.contains(&Flag::TocHeadingBlockSkew)
            && total_entries > heading_skew_severe(profile));
    if severe {
        ShapeSeverity::Severe
    } else if flags.is_empty() {
        ShapeSeverity::Clean
    } else {
        ShapeSeverity::Mild
    }
}

/// Severe-band threshold for heading-block skew. Derived from the
/// profile's `flat_severe_min_entries` (default 10) doubled to recover
/// the legacy 20-entry threshold.
fn heading_skew_severe(profile: &AuditProfile) -> usize {
    profile.toc_shape.flat_severe_min_entries.saturating_mul(2)
}

/// Apply the down-only shape dampening to the per-field verdict and
/// confidence rollup. The verdict can only move toward
/// [`Verdict::NeedsWork`]; the confidence can only move toward
/// [`Confidence::Low`].
fn apply_shape_to_verdict_and_confidence(
    verdict: &mut Verdict,
    confidence: &mut Confidence,
    severity: ShapeSeverity,
) {
    match severity {
        ShapeSeverity::Severe => {
            *verdict = Verdict::NeedsWork;
            *confidence = Confidence::Low;
        }
        ShapeSeverity::Mild => {
            if matches!(*confidence, Confidence::High) {
                *confidence = Confidence::Medium;
            }
        }
        ShapeSeverity::Clean => {}
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

/// True when the effective year still matches the raw biblio's year
/// — i.e. no override has changed it. Only then does it make sense to
/// inspect `biblio.year_raw` for shape signals: an override comes from
/// a human and should not be downgraded by extract-side heuristics.
fn year_came_from_raw_biblio(input: &AuditInput, effective_text: &str) -> bool {
    let Some(raw_year) = input.biblio.year else {
        return false;
    };
    let Ok(effective_year) = effective_text.trim().parse::<i32>() else {
        return false;
    };
    raw_year == effective_year
}

/// True when a date string carries a time component, the canonical
/// shape EPUBs use for build/export timestamps rather than publication
/// dates (`2011-09-29T16:00:00+00:00`, or any value containing `:`).
pub fn looks_like_timestamp(raw: &str) -> bool {
    raw.contains('T') || raw.contains(':')
}

fn audit_title(
    input: &AuditInput,
    profile: &AuditProfile,
    prior: SourcePrior,
    doubtful: bool,
) -> FieldReport {
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
                if profile.title.placeholder_check
                    && is_placeholder(trimmed, &input.data.placeholder_titles)
                {
                    weaken(&mut grade);
                    flags.push(Flag::PlaceholderValue);
                }
                if profile.title.purely_numeric
                    && trimmed.chars().all(|c| c.is_numeric() || c.is_whitespace())
                {
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
                if profile.title.any_bracketed_enabled()
                    && let Some(sub) = classify_bracketed_segment(
                        trimmed,
                        profile.title.bracketed_min_chars,
                        &input.data.volume_suffix_tokens,
                    )
                {
                    let toggle = match sub {
                        BracketSubtype::Series => profile.title.series_paren,
                        BracketSubtype::Marketing => profile.title.marketing_block,
                        BracketSubtype::Aggregator => profile.title.aggregator_marker,
                        BracketSubtype::Volume => profile.title.volume_marker,
                    };
                    if toggle {
                        match sub {
                            BracketSubtype::Series => {
                                weaken(&mut grade);
                                flags.push(Flag::TitleSeriesParen);
                            }
                            BracketSubtype::Marketing => {
                                weaken(&mut grade);
                                flags.push(Flag::TitleMarketingBlock);
                            }
                            BracketSubtype::Aggregator => {
                                weaken(&mut grade);
                                flags.push(Flag::TitleAggregatorMarker);
                            }
                            BracketSubtype::Volume => {
                                // Volume / edition markers are notes,
                                // not weaknesses; flag for visibility
                                // without touching the grade.
                                flags.push(Flag::TitleVolumeMarker);
                            }
                        }
                    }
                }
            }
        }
    }
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some(), profile);
    apply_doubtful(&mut grade, &mut flags, doubtful, value.is_some());

    let hint = title_hint(&flags, grade);
    FieldReport {
        field: "title".to_string(),
        grade,
        flags,
        hint,
    }
}

fn audit_language(
    input: &AuditInput,
    profile: &AuditProfile,
    prior: SourcePrior,
    doubtful: bool,
) -> FieldReport {
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
                if profile.language.bcp47_check && !is_bcp47(trimmed) {
                    weaken(&mut grade);
                    flags.push(Flag::NonBcp47);
                }
                if profile.language.body_script_match
                    && !body_matches_language(trimmed, input.body_sample)
                {
                    weaken(&mut grade);
                    flags.push(Flag::LangMismatchesBody);
                }
            }
        }
    }
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some(), profile);
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

fn audit_publisher(
    input: &AuditInput,
    profile: &AuditProfile,
    prior: SourcePrior,
    doubtful: bool,
) -> FieldReport {
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
                match publishers::evaluate(
                    trimmed,
                    input.data,
                    profile.publisher.url_watermark,
                    profile.publisher.whitelist_normalize_abbreviations,
                ) {
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
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some(), profile);
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

fn audit_year(
    input: &AuditInput,
    profile: &AuditProfile,
    prior: SourcePrior,
    doubtful: bool,
) -> FieldReport {
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
            if profile.year.range_check {
                match text.trim().parse::<i32>() {
                    Err(_) => {
                        weaken(&mut grade);
                        flags.push(Flag::YearOutOfRange);
                    }
                    Ok(year) => {
                        if !(profile.year.min..=profile.year.max).contains(&year) {
                            weaken(&mut grade);
                            flags.push(Flag::YearOutOfRange);
                        }
                    }
                }
            }
            if profile.year.pdf_likely_file_date && pdf_year_unreliable(&input.provenance.adapter) {
                weaken(&mut grade);
                flags.push(Flag::PdfYearLikelyFileDate);
            }
            if profile.year.timestamp_form
                && year_came_from_raw_biblio(input, text)
                && let Some(raw) = input.biblio.year_raw.as_deref()
                && looks_like_timestamp(raw)
            {
                weaken(&mut grade);
                flags.push(Flag::DateLooksLikeTimestamp);
            }
        }
    }
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some(), profile);
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

fn audit_subtitle(
    input: &AuditInput,
    profile: &AuditProfile,
    prior: SourcePrior,
    doubtful: bool,
) -> FieldReport {
    audit_optional(input, profile, prior, doubtful, "subtitle")
}

fn audit_series(
    input: &AuditInput,
    profile: &AuditProfile,
    prior: SourcePrior,
    doubtful: bool,
) -> FieldReport {
    audit_optional(input, profile, prior, doubtful, "series")
}

fn audit_optional(
    input: &AuditInput,
    profile: &AuditProfile,
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
    apply_source_prior(&mut grade, &mut flags, prior, value.is_some(), profile);
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
    profile: &AuditProfile,
) {
    if !present || !profile.source_prior.enabled {
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

fn is_placeholder(value: &str, words: &[String]) -> bool {
    let lower = value.to_lowercase();
    let stripped = lower.trim_matches(|c: char| !c.is_alphanumeric());
    words.iter().any(|w| w.eq_ignore_ascii_case(stripped))
}

/// Bracket subtype the title classifier resolves a leading or trailing
/// bracketed segment into. Each variant maps onto one [`Flag`] and one
/// toggle in [`AuditProfile::title`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BracketSubtype {
    /// Plain bracketed content that reads as a series name (no
    /// sentence-end punctuation, not a volume marker, not an aggregator
    /// header).
    Series,
    /// Marketing copy: lenticular brackets at the tail, or any bracket
    /// whose inner content carries sentence-end punctuation.
    Marketing,
    /// Aggregator / upload marker at the head, in square or lenticular
    /// brackets.
    Aggregator,
    /// Volume or edition marker — bracketed content like `xxx\u{518C}`,
    /// `xxx\u{7248}`, `\u{56FE}\u{6587}\u{7248}`, `\u{5168}xxx\u{672C}`,
    /// or the ASCII token `Indexed`.
    Volume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BracketPair {
    AsciiParen,
    FullwidthParen,
    Square,
    FullwidthSquare,
    Lenticular,
}

impl BracketPair {
    fn delimiters(self) -> (char, char) {
        match self {
            BracketPair::AsciiParen => ('(', ')'),
            BracketPair::FullwidthParen => ('\u{FF08}', '\u{FF09}'),
            BracketPair::Square => ('[', ']'),
            BracketPair::FullwidthSquare => ('\u{FF3B}', '\u{FF3D}'),
            BracketPair::Lenticular => ('\u{3010}', '\u{3011}'),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BracketPosition {
    Head,
    Tail,
}

/// Classify a leading or trailing bracketed segment, returning the
/// subtype so [`audit_title`] can route it to the right toggle and
/// flag. Returns `None` when the title has no qualifying segment.
///
/// Pairs recognised: ASCII `()` / `[]`, fullwidth `（）` / `［］`, and
/// CJK lenticular `\u{3010}\u{3011}`. The opening at byte 0 and the
/// closing at the last char (after trimming) are checked — a bracket
/// in the middle of a title is left alone. Short bracketed fragments
/// (`Foo (v2)`) are tolerated through `min_content_chars`.
fn classify_bracketed_segment(
    trimmed: &str,
    min_content_chars: usize,
    volume_tokens: &[String],
) -> Option<BracketSubtype> {
    const PAIRS: &[BracketPair] = &[
        BracketPair::AsciiParen,
        BracketPair::FullwidthParen,
        BracketPair::Square,
        BracketPair::FullwidthSquare,
        BracketPair::Lenticular,
    ];

    let chars: Vec<char> = trimmed.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let first = chars[0];
    let last = chars[chars.len() - 1];

    for &pair in PAIRS {
        let (open, close) = pair.delimiters();
        // Leading bracket: starts with `open`, has a matching `close`
        // somewhere later, and something follows the closing bracket.
        if first == open
            && let Some(end_idx) = chars.iter().skip(1).position(|&c| c == close)
        {
            let inner: String = chars[1..1 + end_idx].iter().collect();
            let trailing_chars = chars.len().saturating_sub(end_idx + 2);
            if end_idx >= min_content_chars && trailing_chars > 0 {
                return Some(subtype_for(
                    pair,
                    BracketPosition::Head,
                    &inner,
                    volume_tokens,
                ));
            }
        }
        // Trailing bracket: ends with `close`, has a matching `open`
        // somewhere earlier, and something precedes the opening bracket.
        if last == close
            && let Some(start_offset) = chars.iter().rev().skip(1).position(|&c| c == open)
        {
            let open_idx = chars.len() - 2 - start_offset;
            let inner: String = chars[open_idx + 1..chars.len() - 1].iter().collect();
            let leading_chars = open_idx;
            if start_offset >= min_content_chars && leading_chars > 0 {
                return Some(subtype_for(
                    pair,
                    BracketPosition::Tail,
                    &inner,
                    volume_tokens,
                ));
            }
        }
    }
    None
}

/// Pick a [`BracketSubtype`] for one bracketed segment.
///
/// Priority: volume / edition marker first (regardless of bracket
/// shape), then aggregator (square or lenticular at the head), then
/// marketing (lenticular at the tail, or any inner content carrying
/// sentence-end punctuation), else series as the default.
fn subtype_for(
    pair: BracketPair,
    pos: BracketPosition,
    inner: &str,
    volume_tokens: &[String],
) -> BracketSubtype {
    if is_volume_marker(inner, volume_tokens) {
        return BracketSubtype::Volume;
    }
    if pos == BracketPosition::Head
        && matches!(
            pair,
            BracketPair::Square | BracketPair::FullwidthSquare | BracketPair::Lenticular
        )
    {
        return BracketSubtype::Aggregator;
    }
    if pos == BracketPosition::Tail && matches!(pair, BracketPair::Lenticular) {
        return BracketSubtype::Marketing;
    }
    if inner.chars().any(is_sentence_terminator) {
        return BracketSubtype::Marketing;
    }
    BracketSubtype::Series
}

/// Recognise volume / edition markers inside a bracketed segment.
///
/// The ASCII token `Indexed` (used by some English-language indexes) is
/// always recognised. Any other suffix is configurable through
/// `volume_suffix_tokens`: the bracketed content is a volume marker
/// when its trimmed form ends with any of the supplied tokens.
fn is_volume_marker(inner: &str, volume_tokens: &[String]) -> bool {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.eq_ignore_ascii_case("indexed") {
        return true;
    }
    volume_tokens
        .iter()
        .any(|token| !token.is_empty() && trimmed.ends_with(token.as_str()))
}

/// Sentence-end punctuation used to spot marketing copy: CJK fullwidth
/// `\u{3002}` / `\u{FF01}` / `\u{FF1F}` / `\u{2026}` and the ASCII
/// equivalents `.` / `!` / `?`.
fn is_sentence_terminator(c: char) -> bool {
    matches!(
        c,
        '\u{3002}' | '\u{FF01}' | '\u{FF1F}' | '\u{2026}' | '!' | '?'
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
pub fn is_valid_isbn(value: &str) -> bool {
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

    const DEFAULT_BRACKETED_MIN: usize = 3;

    /// CJK volume / edition / printing suffix chars exercised by the
    /// bracketed-segment tests: `\u{518C}`, `\u{7248}`, `\u{672C}`.
    fn default_volume_tokens() -> Vec<String> {
        vec![
            "\u{518C}".to_string(),
            "\u{7248}".to_string(),
            "\u{672C}".to_string(),
        ]
    }

    /// Wrap [`classify_bracketed_segment`] with the default
    /// volume-suffix token set so each test reads as the shape it is
    /// exercising.
    fn classify(title: &str) -> Option<BracketSubtype> {
        classify_bracketed_segment(title, DEFAULT_BRACKETED_MIN, &default_volume_tokens())
    }

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

    #[test]
    fn bracketed_segment_classifies_trailing_series_suffix() {
        // A trailing ASCII-parenthetical block of CJK content without
        // a volume suffix and without sentence-end punctuation reads
        // as a series-marker block.
        let title = include_str!("../tests/fixtures/titles/series_paren.txt").trim();
        assert_eq!(classify(title), Some(BracketSubtype::Series));
    }

    #[test]
    fn bracketed_segment_classifies_fullwidth_volume_marker() {
        // Fullwidth parens whose content ends with a CJK volume
        // suffix resolve to a volume marker rather than a series name.
        let title = include_str!("../tests/fixtures/titles/fullwidth_volume.txt").trim();
        assert_eq!(classify(title), Some(BracketSubtype::Volume));
    }

    #[test]
    fn bracketed_segment_classifies_lenticular_trailing_marketing_block() {
        // Lenticular brackets at the tail are read as marketing copy.
        let title = include_str!("../tests/fixtures/titles/lenticular_marketing.txt").trim();
        assert_eq!(classify(title), Some(BracketSubtype::Marketing));
    }

    #[test]
    fn bracketed_segment_classifies_square_aggregator_head() {
        // Leading square brackets read as an aggregator header.
        let title = include_str!("../tests/fixtures/titles/square_aggregator.txt").trim();
        assert_eq!(classify(title), Some(BracketSubtype::Aggregator));
    }

    #[test]
    fn bracketed_segment_classifies_marketing_by_sentence_punctuation() {
        // A trailing parenthetical block carrying CJK sentence-end
        // punctuation reads as marketing copy, not a series name.
        let title = include_str!("../tests/fixtures/titles/sentence_punct_marketing.txt").trim();
        assert_eq!(classify(title), Some(BracketSubtype::Marketing));
    }

    #[test]
    fn bracketed_segment_tolerates_short_trailing_marker() {
        // `Foo (v2)` — short bracketed content is left alone.
        assert_eq!(classify("Foo (v2)"), None);
        // `A Book` — no brackets at all.
        assert_eq!(classify("A Book"), None);
    }

    #[test]
    fn bracketed_segment_ignores_brackets_in_the_middle() {
        assert_eq!(classify("Foo (1990) Bar"), None);
    }

    #[test]
    fn bracketed_segment_requires_text_outside_the_brackets() {
        assert_eq!(classify("(everything inside)"), None);
        assert_eq!(classify("[everything inside]"), None);
    }

    #[test]
    fn bracketed_segment_classifies_ascii_indexed_volume_marker() {
        // `Long Title (Indexed)` — ASCII volume marker.
        assert_eq!(
            classify("A Long Title (Indexed)"),
            Some(BracketSubtype::Volume)
        );
    }

    #[test]
    fn timestamp_shape_detection() {
        assert!(looks_like_timestamp("2011-09-29T16:00:00+00:00"));
        assert!(looks_like_timestamp("2009-09-28T00:00:00Z"));
        assert!(looks_like_timestamp("2019-07-25 14:30:00"));
        assert!(!looks_like_timestamp("2010"));
        assert!(!looks_like_timestamp("2010-05"));
        assert!(!looks_like_timestamp("2010-05-15"));
    }
}
