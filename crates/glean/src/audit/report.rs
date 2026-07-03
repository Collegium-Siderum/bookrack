// SPDX-License-Identifier: Apache-2.0

//! Audit report types: [`PaperReport`], [`PaperFieldReport`],
//! [`PaperFlag`], and the three grade enums [`PaperFieldGrade`],
//! [`PaperVerdict`], [`PaperConfidence`].
//!
//! The three grade enums mirror the books pipeline's `FieldGrade`,
//! `Verdict`, `Confidence` exactly at the token level — `as_token()`
//! returns the same snake_case strings — so the values round-trip
//! through `node_publication_attrs.audit_verdict` and `confidence`
//! columns without a per-pipeline schema. The types are nevertheless
//! distinct so a change in one pipeline cannot quietly reach the
//! other.

use std::collections::BTreeMap;

/// How strong the evidence for a single field's value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperFieldGrade {
    /// No effective value at all.
    Missing,
    /// Present but flagged for at least one weakness.
    Weak,
    /// Present and clean of weaknesses, but without an upgrading
    /// signal.
    Medium,
    /// Present, clean, and corroborated by an upgrading signal.
    Strong,
}

impl PaperFieldGrade {
    /// A short token for the grade, suitable for logs and JSON.
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Weak => "weak",
            Self::Medium => "medium",
            Self::Strong => "strong",
        }
    }
}

/// Aggregate verdict the audit assigns to one paper.
///
/// `Clean` means no required field is below Medium. `NeedsWork`
/// means at least one required field is Missing or Weak, or a
/// cross-field signal (e.g. [`PaperFlag::NoStableIdentifier`])
/// floored it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperVerdict {
    Clean,
    NeedsWork,
}

impl PaperVerdict {
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::NeedsWork => "needs_work",
        }
    }
}

/// Row-level confidence on the rolled-up record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperConfidence {
    Low,
    Medium,
    High,
}

impl PaperConfidence {
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// A signal the audit emitted while grading a paper. Variants are
/// closed — paper-shape only — so a change to the book audit's
/// `Flag` enum cannot reach glean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperFlag {
    // ── identifier format / checksum ──────────────────────────────
    /// DOI did not match the Crossref-recommended format.
    DoiInvalidFormat,
    /// arXiv id did not match the old or new canonical form.
    ArxivIdInvalidFormat,
    /// ISSN failed the MOD-11 checksum.
    IssnInvalidChecksum,
    /// At least one contributor's ORCID failed the ISO/IEC 7064
    /// MOD 11-2 checksum.
    OrcidInvalidChecksum,

    // ── identifier strength (cross-field) ─────────────────────────
    /// No DOI, no arXiv id, and no (ISSN + container_title) — the
    /// paper has no stable identifier the catalog can pin on.
    NoStableIdentifier,

    // ── generic field signals ─────────────────────────────────────
    /// The field has no effective value at all.
    Empty,
    /// The field is voided (curator suppressed the extracted value
    /// and set no replacement).
    Voided,
    /// The field's value matches an entry in
    /// [`crate::audit::PaperAuditData::placeholder_titles`].
    PlaceholderValue,
    /// The field's value equals the source filename's stem.
    EqualsFilename,
    /// The field's value contains a watermark token from
    /// [`crate::audit::PaperAuditData::watermark_tokens`].
    SourceWatermark,
    /// The field reads as a pure numeric string with no alphabetic
    /// content — usually a stray identifier substring.
    PurelyNumeric,
    /// Year falls outside `[profile.year.min, profile.year.max]`.
    YearOutOfRange,
    /// The raw date carries a time component (`T::`-shaped) so the
    /// year is more likely a file build / export stamp.
    DateLooksLikeTimestamp,
    /// The raw date matches the PDF `/Info CreationDate` shape
    /// (`D:YYYYMMDD…`) and is therefore more likely a file date
    /// than a publication year.
    PdfYearLikelyFileDate,
    /// The declared language's script disagrees with the body
    /// sample.
    LangMismatchesBody,
    /// The declared language is not a recognised BCP-47 primary
    /// subtag.
    NonBcp47,
    /// The source format gives a weak prior on extraction fidelity
    /// (e.g. plain text without typographic cues).
    SourcePriorWeak,
    /// The text layer was tagged `Doubtful` by the extractor.
    DoubtfulTextLayer,

    // ── paper-shape signals ───────────────────────────────────────
    /// Abstract is shorter than
    /// [`crate::audit::profile::AbstractToggles::min_chars`].
    AbstractTooShort,
    /// Container title is not in
    /// [`crate::audit::PaperAuditData::venue_whitelist`].
    VenueNotInList,
    /// Contributor list is empty.
    AuthorListEmpty,
    /// At least one contributor has a single-word display name
    /// (often a parsing artefact — only the surname or only the
    /// initials made it through).
    AuthorListSingleWord,
    /// Title starts with `arxiv:` or `arxiv `, echoing the banner
    /// the extractor copied off page one.
    TitleEchoesArxivBanner,
    /// At least one contributor name matches the configured
    /// sentinel list (e.g. `Editorial Board`, `Anonymous`).
    ContributorSentinelName,
}

impl PaperFlag {
    /// Every [`PaperFlag`] variant, in declaration order. The
    /// `node_paper_audit` writer enumerates this slice to set
    /// per-flag columns; the order is the canonical token order.
    pub const ALL: &'static [PaperFlag] = &[
        PaperFlag::DoiInvalidFormat,
        PaperFlag::ArxivIdInvalidFormat,
        PaperFlag::IssnInvalidChecksum,
        PaperFlag::OrcidInvalidChecksum,
        PaperFlag::NoStableIdentifier,
        PaperFlag::Empty,
        PaperFlag::Voided,
        PaperFlag::PlaceholderValue,
        PaperFlag::EqualsFilename,
        PaperFlag::SourceWatermark,
        PaperFlag::PurelyNumeric,
        PaperFlag::YearOutOfRange,
        PaperFlag::DateLooksLikeTimestamp,
        PaperFlag::PdfYearLikelyFileDate,
        PaperFlag::LangMismatchesBody,
        PaperFlag::NonBcp47,
        PaperFlag::SourcePriorWeak,
        PaperFlag::DoubtfulTextLayer,
        PaperFlag::AbstractTooShort,
        PaperFlag::VenueNotInList,
        PaperFlag::AuthorListEmpty,
        PaperFlag::AuthorListSingleWord,
        PaperFlag::TitleEchoesArxivBanner,
        PaperFlag::ContributorSentinelName,
    ];

    /// A stable snake_case token for the flag, suitable for JSON
    /// output and structured logs.
    pub fn as_token(self) -> &'static str {
        match self {
            Self::DoiInvalidFormat => "doi_invalid_format",
            Self::ArxivIdInvalidFormat => "arxiv_id_invalid_format",
            Self::IssnInvalidChecksum => "issn_invalid_checksum",
            Self::OrcidInvalidChecksum => "orcid_invalid_checksum",
            Self::NoStableIdentifier => "no_stable_identifier",
            Self::Empty => "empty",
            Self::Voided => "voided",
            Self::PlaceholderValue => "placeholder_value",
            Self::EqualsFilename => "equals_filename",
            Self::SourceWatermark => "source_watermark",
            Self::PurelyNumeric => "purely_numeric",
            Self::YearOutOfRange => "year_out_of_range",
            Self::DateLooksLikeTimestamp => "date_looks_like_timestamp",
            Self::PdfYearLikelyFileDate => "pdf_year_likely_file_date",
            Self::LangMismatchesBody => "lang_mismatches_body",
            Self::NonBcp47 => "non_bcp47",
            Self::SourcePriorWeak => "source_prior_weak",
            Self::DoubtfulTextLayer => "doubtful_text_layer",
            Self::AbstractTooShort => "abstract_too_short",
            Self::VenueNotInList => "venue_not_in_list",
            Self::AuthorListEmpty => "author_list_empty",
            Self::AuthorListSingleWord => "author_list_single_word",
            Self::TitleEchoesArxivBanner => "title_echoes_arxiv_banner",
            Self::ContributorSentinelName => "contributor_sentinel_name",
        }
    }
}

/// Per-field audit outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct PaperFieldReport {
    pub grade: PaperFieldGrade,
    pub flags: Vec<PaperFlag>,
    /// Optional, free-form hint shown alongside the grade in the
    /// JSON notes. `None` when no hint applies.
    pub hint: Option<String>,
}

impl PaperFieldReport {
    pub fn new(grade: PaperFieldGrade) -> Self {
        Self {
            grade,
            flags: Vec::new(),
            hint: None,
        }
    }

    pub fn weaken_to(&mut self, target: PaperFieldGrade, flag: PaperFlag) {
        if grade_rank(target) < grade_rank(self.grade) {
            self.grade = target;
        }
        if !self.flags.contains(&flag) {
            self.flags.push(flag);
        }
    }

    pub fn push_flag(&mut self, flag: PaperFlag) {
        if !self.flags.contains(&flag) {
            self.flags.push(flag);
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Full audit report for one paper.
#[derive(Debug, Clone, PartialEq)]
pub struct PaperReport {
    /// Per-field outcomes, keyed by field name (e.g. `"title"`,
    /// `"doi"`, `"abstract"`). Field-name keys are stable strings
    /// suitable for JSON.
    pub fields: BTreeMap<&'static str, PaperFieldReport>,
    pub verdict: PaperVerdict,
    pub confidence: PaperConfidence,
    /// Cross-field flags that do not belong to a single field —
    /// e.g. [`PaperFlag::NoStableIdentifier`].
    pub cross_field_flags: Vec<PaperFlag>,
}

impl PaperReport {
    /// Render the report as a compact JSON string. The schema is:
    ///
    /// ```json
    /// {
    ///   "verdict": "clean" | "needs_work",
    ///   "confidence": "high" | "medium" | "low",
    ///   "cross_field_flags": ["no_stable_identifier", ...],
    ///   "fields": {
    ///     "title": {
    ///       "grade": "strong",
    ///       "flags": ["placeholder_value"],
    ///       "hint": "matches placeholder list"
    ///     },
    ///     ...
    ///   }
    /// }
    /// ```
    ///
    /// Keys are emitted in field-name order; arrays preserve flag
    /// order so the output is byte-stable across runs over the same
    /// input.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push('{');
        write_kv(&mut out, "verdict", &json_str(self.verdict.as_token()));
        out.push(',');
        write_kv(
            &mut out,
            "confidence",
            &json_str(self.confidence.as_token()),
        );
        out.push(',');
        out.push_str("\"cross_field_flags\":");
        out.push_str(&json_flag_array(&self.cross_field_flags));
        out.push_str(",\"fields\":{");
        let mut first = true;
        for (name, field) in &self.fields {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&json_str(name));
            out.push_str(":{");
            write_kv(&mut out, "grade", &json_str(field.grade.as_token()));
            out.push(',');
            out.push_str("\"flags\":");
            out.push_str(&json_flag_array(&field.flags));
            if let Some(hint) = &field.hint {
                out.push(',');
                write_kv(&mut out, "hint", &json_str(hint));
            }
            out.push('}');
        }
        out.push_str("}}");
        out
    }
}

fn grade_rank(g: PaperFieldGrade) -> u8 {
    match g {
        PaperFieldGrade::Missing => 0,
        PaperFieldGrade::Weak => 1,
        PaperFieldGrade::Medium => 2,
        PaperFieldGrade::Strong => 3,
    }
}

fn write_kv(out: &mut String, key: &str, value: &str) {
    out.push_str(&json_str(key));
    out.push(':');
    out.push_str(value);
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_flag_array(flags: &[PaperFlag]) -> String {
    let mut out = String::from("[");
    for (i, f) in flags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_str(f.as_token()));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grade_tokens_match_book_audit_tokens() {
        assert_eq!(PaperFieldGrade::Missing.as_token(), "missing");
        assert_eq!(PaperFieldGrade::Weak.as_token(), "weak");
        assert_eq!(PaperFieldGrade::Medium.as_token(), "medium");
        assert_eq!(PaperFieldGrade::Strong.as_token(), "strong");
    }

    #[test]
    fn verdict_and_confidence_tokens_match_catalog_schema() {
        assert_eq!(PaperVerdict::Clean.as_token(), "clean");
        assert_eq!(PaperVerdict::NeedsWork.as_token(), "needs_work");
        assert_eq!(PaperConfidence::Low.as_token(), "low");
        assert_eq!(PaperConfidence::Medium.as_token(), "medium");
        assert_eq!(PaperConfidence::High.as_token(), "high");
    }

    #[test]
    fn flag_tokens_are_stable_snake_case() {
        // Spot-check a representative slice rather than every variant.
        assert_eq!(PaperFlag::DoiInvalidFormat.as_token(), "doi_invalid_format");
        assert_eq!(
            PaperFlag::NoStableIdentifier.as_token(),
            "no_stable_identifier"
        );
        assert_eq!(PaperFlag::AbstractTooShort.as_token(), "abstract_too_short");
        assert_eq!(
            PaperFlag::ContributorSentinelName.as_token(),
            "contributor_sentinel_name"
        );
    }

    #[test]
    fn weaken_to_lowers_grade_and_appends_flag_idempotently() {
        let mut report = PaperFieldReport::new(PaperFieldGrade::Strong);
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::PlaceholderValue);
        assert_eq!(report.grade, PaperFieldGrade::Weak);
        assert_eq!(report.flags, vec![PaperFlag::PlaceholderValue]);
        // A second call with the same flag does not duplicate it.
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::PlaceholderValue);
        assert_eq!(report.flags, vec![PaperFlag::PlaceholderValue]);
        // A higher target grade is ignored.
        report.weaken_to(PaperFieldGrade::Medium, PaperFlag::PlaceholderValue);
        assert_eq!(report.grade, PaperFieldGrade::Weak);
    }

    #[test]
    fn report_json_is_stable_and_contains_every_section() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "title",
            PaperFieldReport {
                grade: PaperFieldGrade::Weak,
                flags: vec![PaperFlag::PlaceholderValue],
                hint: Some("matches placeholder list".to_string()),
            },
        );
        fields.insert(
            "year",
            PaperFieldReport {
                grade: PaperFieldGrade::Strong,
                flags: Vec::new(),
                hint: None,
            },
        );
        let report = PaperReport {
            fields,
            verdict: PaperVerdict::NeedsWork,
            confidence: PaperConfidence::Low,
            cross_field_flags: vec![PaperFlag::NoStableIdentifier],
        };
        let json = report.to_json();
        assert!(json.contains("\"verdict\":\"needs_work\""));
        assert!(json.contains("\"confidence\":\"low\""));
        assert!(json.contains("\"cross_field_flags\":[\"no_stable_identifier\"]"));
        assert!(json.contains("\"title\":{\"grade\":\"weak\","));
        assert!(json.contains("\"flags\":[\"placeholder_value\"]"));
        assert!(json.contains("\"hint\":\"matches placeholder list\""));
        assert!(json.contains("\"year\":{\"grade\":\"strong\",\"flags\":[]}"));
        // No trailing junk and ends in matched braces.
        assert!(json.ends_with("}}"));
    }

    #[test]
    fn json_str_escapes_control_and_line_separators() {
        assert_eq!(json_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_str("a\nb\rc\td"), "\"a\\nb\\rc\\td\"");
        assert_eq!(json_str("a\u{0}b"), "\"a\\u0000b\"");
        // U+2028 / U+2029 are legal JSON but break JavaScript string
        // literals, so they are escaped rather than emitted verbatim.
        assert_eq!(json_str("a\u{2028}b\u{2029}c"), "\"a\\u2028b\\u2029c\"");
    }
}
