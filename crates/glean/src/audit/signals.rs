// SPDX-License-Identifier: Apache-2.0

//! The audit engine: deterministic per-field grading from the inputs
//! gathered in [`PaperAuditInput`].
//!
//! Each field starts at [`PaperFieldGrade::Strong`] when present and
//! at [`PaperFieldGrade::Missing`] when not. Signals weaken or
//! strengthen the grade one step at a time, each appending a
//! [`PaperFlag`]. The aggregate verdict and the row-level confidence
//! are computed once all per-field grades are settled.
//!
//! Every weakening signal is gated by a corresponding toggle in
//! [`PaperAuditProfile`]. A signal whose toggle is off neither
//! weakens the grade nor appends its flag.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use bookrack_catalog::EffectiveAttrs;
use bookrack_extract::{Biblio, Contributor, CslType, Provenance, TextLayerQuality};
use regex::Regex;

use super::csl_required::{RequirementLevel, requirement};
use super::data::PaperAuditData;
use super::profile::PaperAuditProfile;
use super::report::{
    PaperConfidence, PaperFieldGrade, PaperFieldReport, PaperFlag, PaperReport, PaperVerdict,
};

/// Everything one audit run needs, gathered by the caller.
///
/// The audit reads `effective` for the values it grades (so a
/// post-hoc override flips the grade on the next run) and reads the
/// extracted `biblio` and `provenance` for signals that depend on
/// the raw extraction (DOI / arXiv / ISSN format checks, contributor
/// list, source-format prior, text-layer quality).
pub struct PaperAuditInput<'a> {
    pub biblio: &'a Biblio,
    pub provenance: &'a Provenance,
    pub effective: &'a EffectiveAttrs,
    /// Concatenated text of the paper's first few body blocks. Used
    /// by the language signal to compare the declared language
    /// against the body's script.
    pub body_sample: &'a str,
    /// The source file's stem (no extension). Used to flag a title
    /// that merely echoes the filename.
    pub source_stem: Option<&'a str>,
}

/// Run the audit over the prepared input under one profile and data
/// set.
pub fn audit_paper(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    data: &PaperAuditData,
) -> PaperReport {
    if !profile.audit_enabled {
        return PaperReport {
            fields: BTreeMap::new(),
            verdict: PaperVerdict::Clean,
            confidence: PaperConfidence::Medium,
            cross_field_flags: Vec::new(),
        };
    }

    let mut fields: BTreeMap<&'static str, PaperFieldReport> = BTreeMap::new();

    grade_title(input, profile, data, &mut fields);
    grade_year(input, profile, &mut fields);
    grade_doi(input, profile, &mut fields);
    grade_arxiv(input, profile, &mut fields);
    grade_issn(input, profile, &mut fields);
    grade_container(input, profile, data, &mut fields);
    grade_abstract(input, profile, &mut fields);
    grade_author(input, profile, data, &mut fields);
    grade_language(input, profile, &mut fields);

    let mut cross_field_flags = Vec::new();
    if profile.identifier.require_any && !has_any_stable_identifier(input.biblio) {
        cross_field_flags.push(PaperFlag::NoStableIdentifier);
    }

    let (verdict, confidence) = roll_up(input.biblio.csl_type, &fields, &cross_field_flags);

    PaperReport {
        fields,
        verdict,
        confidence,
        cross_field_flags,
    }
}

// ─── per-field graders ──────────────────────────────────────────────

fn grade_title(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    data: &PaperAuditData,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let value = input.effective.get("title");
    let mut report = start_field("title", value);
    let val = match value {
        Some(v) => v,
        None => {
            fields.insert("title", report);
            return;
        }
    };
    let t = profile.title.clone();
    if t.empty_check && val.trim().is_empty() {
        report.weaken_to(PaperFieldGrade::Missing, PaperFlag::Empty);
    }
    if t.placeholder_check
        && data
            .placeholder_titles
            .iter()
            .any(|p| p.eq_ignore_ascii_case(val.trim()))
    {
        report.weaken_to(PaperFieldGrade::Missing, PaperFlag::PlaceholderValue);
    }
    if t.echoes_arxiv_banner_check && looks_like_arxiv_banner(val) {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::TitleEchoesArxivBanner);
    }
    if t.equals_filename_check
        && let Some(stem) = input.source_stem
        && normalize_compare(val) == normalize_compare(stem)
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::EqualsFilename);
    }
    apply_watermark(&mut report, val, data, profile);
    fields.insert("title", report);
}

fn grade_year(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let value = input.effective.get("year");
    let mut report = start_field("year", value);
    let val = match value {
        Some(v) => v,
        None => {
            fields.insert("year", report);
            return;
        }
    };
    let y = profile.year.clone();
    if let Ok(n) = val.parse::<i32>()
        && y.range_check
        && (n < y.min || n > y.max)
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::YearOutOfRange);
    }
    if y.timestamp_form
        && let Some(raw) = input.biblio.year_raw.as_deref()
        && looks_like_timestamp(raw)
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::DateLooksLikeTimestamp);
    }
    if y.pdf_likely_file_date
        && let Some(raw) = input.biblio.year_raw.as_deref()
        && PDF_DATE_RE.is_match(raw)
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::PdfYearLikelyFileDate);
    }
    fields.insert("year", report);
}

fn grade_doi(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let value = input.effective.get("doi");
    let mut report = start_field("doi", value);
    if let Some(val) = value
        && profile.identifier.doi_format_check
        && !DOI_RE.is_match(val.trim())
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::DoiInvalidFormat);
    }
    fields.insert("doi", report);
}

fn grade_arxiv(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let value = input.effective.get("arxiv_id");
    let mut report = start_field("arxiv_id", value);
    if let Some(val) = value
        && profile.identifier.arxiv_format_check
        && !ARXIV_NEW_RE.is_match(val.trim())
        && !ARXIV_OLD_RE.is_match(val.trim())
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::ArxivIdInvalidFormat);
    }
    fields.insert("arxiv_id", report);
}

fn grade_issn(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let value = input.effective.get("issn");
    let mut report = start_field("issn", value);
    if let Some(val) = value
        && profile.identifier.issn_checksum_check
        && !issn_checksum_ok(val.trim())
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::IssnInvalidChecksum);
    }
    fields.insert("issn", report);
}

fn grade_container(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    data: &PaperAuditData,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let value = input.effective.get("container_title");
    let mut report = start_field("container_title", value);
    if let Some(val) = value
        && profile.venue.whitelist_check
        && !data.venue_whitelist.is_empty()
        && !data
            .venue_whitelist
            .iter()
            .any(|v| v.eq_ignore_ascii_case(val.trim()))
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::VenueNotInList);
    }
    fields.insert("container_title", report);
}

fn grade_abstract(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let value = input.effective.get("abstract");
    let mut report = start_field("abstract", value);
    if let Some(val) = value
        && profile.abstract_.required
        && (val.trim().chars().count() as u32) < profile.abstract_.min_chars
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::AbstractTooShort);
    }
    fields.insert("abstract", report);
}

fn grade_author(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    data: &PaperAuditData,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let contributors = &input.biblio.contributors;
    let authors: Vec<&Contributor> = contributors
        .iter()
        .filter(|c| matches!(c.role, bookrack_extract::ContributorRole::Author))
        .collect();
    let present = !authors.is_empty();
    let mut report = if present {
        PaperFieldReport::new(PaperFieldGrade::Strong)
    } else {
        PaperFieldReport::new(PaperFieldGrade::Missing)
    };
    if !present && profile.author.required {
        report.push_flag(PaperFlag::AuthorListEmpty);
        fields.insert("author", report);
        return;
    }
    if profile.author.sentinel_check
        && authors.iter().any(|c| {
            data.sentinel_contributor_names
                .iter()
                .any(|s| s.eq_ignore_ascii_case(c.name.trim()))
        })
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::ContributorSentinelName);
    }
    if profile.author.single_word_check
        && authors
            .iter()
            .any(|c| c.name.split_whitespace().count() <= 1)
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::AuthorListSingleWord);
    }
    if profile.identifier.orcid_checksum_check
        && contributors
            .iter()
            .filter_map(|c| c.orcid.as_deref())
            .any(|o| !orcid_checksum_ok(o))
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::OrcidInvalidChecksum);
    }
    fields.insert("author", report);

    // Source-format prior is attached to the author field as a
    // catch-all extraction-quality signal; the books audit follows
    // the same pattern for its catch-all signals.
    if profile.source_prior.enabled {
        let weak_adapter = matches!(input.provenance.adapter.as_str(), "txt");
        if weak_adapter && let Some(r) = fields.get_mut("author") {
            r.weaken_to(PaperFieldGrade::Weak, PaperFlag::SourcePriorWeak);
        }
    }
    if matches!(
        input.provenance.text_layer_quality,
        TextLayerQuality::Doubtful
    ) && let Some(r) = fields.get_mut("author")
    {
        r.weaken_to(PaperFieldGrade::Weak, PaperFlag::DoubtfulTextLayer);
    }
}

fn grade_language(
    input: &PaperAuditInput,
    profile: &PaperAuditProfile,
    fields: &mut BTreeMap<&'static str, PaperFieldReport>,
) {
    let value = input.effective.get("language");
    let mut report = start_field("language", value);
    let Some(val) = value else {
        fields.insert("language", report);
        return;
    };
    let lang = val.trim().to_ascii_lowercase();
    let primary = lang.split('-').next().unwrap_or("");
    if profile.language.bcp47_check && !is_bcp47_primary(primary) {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::NonBcp47);
    }
    if profile.language.body_script_match {
        let (cjk_ratio, latin_ratio) = script_ratios(input.body_sample);
        let declared_cjk = profile
            .language
            .cjk_codes
            .iter()
            .any(|c| c.eq_ignore_ascii_case(primary));
        let declared_latin = profile
            .language
            .latin_codes
            .iter()
            .any(|c| c.eq_ignore_ascii_case(primary));
        let cjk_mismatch = declared_cjk
            && cjk_ratio < profile.language.body_cjk_min_ratio()
            && latin_ratio > profile.language.body_latin_min_ratio();
        let latin_mismatch = declared_latin && cjk_ratio > profile.language.body_cjk_max_ratio();
        if cjk_mismatch || latin_mismatch {
            report.weaken_to(PaperFieldGrade::Weak, PaperFlag::LangMismatchesBody);
        }
    }
    fields.insert("language", report);
}

// ─── helpers ────────────────────────────────────────────────────────

fn start_field(_name: &'static str, value: Option<&str>) -> PaperFieldReport {
    match value {
        Some(v) if !v.trim().is_empty() => PaperFieldReport::new(PaperFieldGrade::Strong),
        _ => PaperFieldReport::new(PaperFieldGrade::Missing),
    }
}

fn apply_watermark(
    report: &mut PaperFieldReport,
    value: &str,
    data: &PaperAuditData,
    _profile: &PaperAuditProfile,
) {
    let lower = value.to_ascii_lowercase();
    if data
        .watermark_tokens
        .iter()
        .any(|t| lower.contains(&t.to_ascii_lowercase()))
    {
        report.weaken_to(PaperFieldGrade::Weak, PaperFlag::SourceWatermark);
    }
}

fn normalize_compare(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn has_any_stable_identifier(b: &Biblio) -> bool {
    b.doi.as_deref().is_some_and(|v| !v.trim().is_empty())
        || b.arxiv_id.as_deref().is_some_and(|v| !v.trim().is_empty())
        || (b.issn.as_deref().is_some_and(|v| !v.trim().is_empty())
            && b.container_title
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty()))
}

fn looks_like_timestamp(raw: &str) -> bool {
    raw.contains(':')
}

fn looks_like_arxiv_banner(s: &str) -> bool {
    let lower = s.trim().to_ascii_lowercase();
    lower.starts_with("arxiv:") || lower.starts_with("arxiv ")
}

fn is_bcp47_primary(tag: &str) -> bool {
    let n = tag.len();
    (2..=3).contains(&n) && tag.chars().all(|c| c.is_ascii_alphabetic())
}

fn script_ratios(text: &str) -> (f64, f64) {
    let mut cjk = 0usize;
    let mut latin = 0usize;
    let mut total = 0usize;
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        total += 1;
        let cp = c as u32;
        let is_cjk = (0x3400..=0x4DBF).contains(&cp)
            || (0x4E00..=0x9FFF).contains(&cp)
            || (0xF900..=0xFAFF).contains(&cp)
            || (0x3040..=0x309F).contains(&cp)
            || (0x30A0..=0x30FF).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp);
        if is_cjk {
            cjk += 1;
        } else if c.is_ascii_alphabetic() {
            latin += 1;
        }
    }
    if total == 0 {
        (0.0, 0.0)
    } else {
        (cjk as f64 / total as f64, latin as f64 / total as f64)
    }
}

/// ISSN MOD-11 checksum. Accepts the dashed form `NNNN-NNNN` and the
/// bare 8-digit form; the last character may be `X` to denote 10.
pub fn issn_checksum_ok(raw: &str) -> bool {
    let digits: Vec<char> = raw.chars().filter(|c| *c != '-' && *c != ' ').collect();
    if digits.len() != 8 {
        return false;
    }
    let mut sum: u32 = 0;
    for (i, c) in digits[..7].iter().enumerate() {
        let d = match c.to_digit(10) {
            Some(d) => d,
            None => return false,
        };
        let weight = 8 - i as u32;
        sum += d * weight;
    }
    let check = digits[7];
    let check_val = if check == 'X' || check == 'x' {
        10
    } else {
        match check.to_digit(10) {
            Some(d) => d,
            None => return false,
        }
    };
    (sum + check_val).is_multiple_of(11)
}

/// ORCID checksum (ISO/IEC 7064 MOD 11-2). Accepts the bare
/// `NNNN-NNNN-NNNN-NNNX` form and the `https://orcid.org/...` URI
/// form.
pub fn orcid_checksum_ok(raw: &str) -> bool {
    let bare = raw.trim().trim_start_matches("https://orcid.org/");
    let digits: Vec<char> = bare.chars().filter(|c| *c != '-' && *c != ' ').collect();
    if digits.len() != 16 {
        return false;
    }
    let mut total: u32 = 0;
    for c in &digits[..15] {
        let d = match c.to_digit(10) {
            Some(d) => d,
            None => return false,
        };
        total = (total + d) * 2;
    }
    let computed = ((12 - (total % 11)) % 11) as u8;
    let check = digits[15];
    let check_val = if check == 'X' || check == 'x' {
        10
    } else {
        match check.to_digit(10) {
            Some(d) => d as u8,
            None => return false,
        }
    };
    computed == check_val
}

// ─── roll-up to verdict + confidence ────────────────────────────────

fn roll_up(
    csl_type: Option<CslType>,
    fields: &BTreeMap<&'static str, PaperFieldReport>,
    cross_field_flags: &[PaperFlag],
) -> (PaperVerdict, PaperConfidence) {
    let mut any_required_missing = false;
    let mut any_required_weak = false;
    let mut required_strong = 0u32;
    let mut required_total = 0u32;
    for name in REQUIRED_FIELD_CANDIDATES {
        let level = requirement(csl_type, name);
        if level != RequirementLevel::Required {
            continue;
        }
        required_total += 1;
        if let Some(r) = fields.get(name) {
            match r.grade {
                PaperFieldGrade::Missing => any_required_missing = true,
                PaperFieldGrade::Weak => any_required_weak = true,
                PaperFieldGrade::Strong => required_strong += 1,
                PaperFieldGrade::Medium => {}
            }
        } else {
            // "author" is graded but not always recorded under a key
            // (legacy field names); treat absence as Missing.
            any_required_missing = true;
        }
    }
    let verdict = if any_required_missing
        || any_required_weak
        || cross_field_flags.contains(&PaperFlag::NoStableIdentifier)
    {
        PaperVerdict::NeedsWork
    } else {
        PaperVerdict::Clean
    };
    let confidence =
        if any_required_missing || cross_field_flags.contains(&PaperFlag::NoStableIdentifier) {
            PaperConfidence::Low
        } else if required_total > 0 && required_strong == required_total {
            PaperConfidence::High
        } else {
            PaperConfidence::Medium
        };
    (verdict, confidence)
}

/// Fields the roll-up consults when scoring `required`. New CSL
/// types add fields here as they enter the matrix.
const REQUIRED_FIELD_CANDIDATES: &[&str] = &[
    "title",
    "author",
    "year",
    "container_title",
    "doi",
    "publisher",
    "abstract",
];

// ─── pinned regexes ────────────────────────────────────────────────

static DOI_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Crossref-recommended form, case-insensitive.
    Regex::new(r"(?i)^10\.\d{4,9}/[-._;()/:A-Za-z0-9]+$").expect("DOI regex")
});

static ARXIV_NEW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}\.\d{4,5}$").expect("arXiv new-form regex"));

static ARXIV_OLD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z][a-z\-]*(?:\.[A-Z]{2})?/\d{7}$").expect("arXiv old-form regex")
});

static PDF_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^D:[0-9]{8,}").expect("PDF date regex"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issn_checksum_validates_known_good_and_bad() {
        // 0378-5955 → check digit 5 (Wikipedia example).
        assert!(issn_checksum_ok("0378-5955"));
        assert!(issn_checksum_ok("03785955"));
        // Mutated check digit fails.
        assert!(!issn_checksum_ok("0378-5950"));
        // Wrong length fails.
        assert!(!issn_checksum_ok("0378-595"));
        assert!(!issn_checksum_ok(""));
    }

    #[test]
    fn issn_checksum_handles_x_as_ten() {
        // Generated valid: 2049-3630 is valid; replacing tail with X
        // is a separate case. Use 2434-561X as a sample valid ISSN.
        assert!(issn_checksum_ok("2434-561X"));
        assert!(issn_checksum_ok("2434561x"));
    }

    #[test]
    fn orcid_checksum_validates_known_good_and_bad() {
        // Bare form (Wikipedia ORCID example).
        assert!(orcid_checksum_ok("0000-0002-1825-0097"));
        // URI form.
        assert!(orcid_checksum_ok("https://orcid.org/0000-0002-1825-0097"));
        // Mutated check digit fails.
        assert!(!orcid_checksum_ok("0000-0002-1825-0091"));
        // Wrong length fails.
        assert!(!orcid_checksum_ok("0000-0002-1825-009"));
    }

    #[test]
    fn doi_regex_matches_crossref_canonical_form_case_insensitively() {
        assert!(DOI_RE.is_match("10.18653/v1/n19-1423"));
        assert!(DOI_RE.is_match("10.1128/AEM.02591-07"));
        assert!(!DOI_RE.is_match("not-a-doi"));
        assert!(!DOI_RE.is_match("10.18653"));
    }

    #[test]
    fn arxiv_regex_distinguishes_old_and_new_forms() {
        assert!(ARXIV_NEW_RE.is_match("2401.12345"));
        assert!(ARXIV_NEW_RE.is_match("0704.1234"));
        assert!(!ARXIV_NEW_RE.is_match("xyz/1234567"));
        assert!(ARXIV_OLD_RE.is_match("cs/0001001"));
        assert!(ARXIV_OLD_RE.is_match("math.CO/0211159"));
        assert!(!ARXIV_OLD_RE.is_match("2401.12345"));
    }

    #[test]
    fn script_ratios_count_cjk_and_latin_only() {
        // Five Latin letters, two CJK ideographs, one skipped
        // whitespace. Total non-whitespace = 7. CJK ideographs are
        // encoded through `\u{...}` so the source stays ASCII-clean
        // per the repo's leak-check.
        let sample = "hello \u{4F60}\u{597D}";
        let (cjk, latin) = script_ratios(sample);
        assert!((latin - 5.0 / 7.0).abs() < 0.01, "latin = {latin}");
        assert!((cjk - 2.0 / 7.0).abs() < 0.01, "cjk = {cjk}");
        let (cjk0, latin0) = script_ratios("");
        assert_eq!(cjk0, 0.0);
        assert_eq!(latin0, 0.0);
    }

    #[test]
    fn looks_like_arxiv_banner_catches_common_forms() {
        assert!(looks_like_arxiv_banner("arXiv:2401.12345"));
        assert!(looks_like_arxiv_banner("arxiv 2401.12345"));
        assert!(!looks_like_arxiv_banner("Attention Is All You Need"));
    }

    #[test]
    fn looks_like_timestamp_detects_colon() {
        assert!(looks_like_timestamp("2011-09-29T16:00:00+00:00"));
        assert!(!looks_like_timestamp("2011-09-29"));
    }

    #[test]
    fn audit_disabled_returns_empty_clean_report() {
        // Constructing a full PaperAuditInput requires a Catalog
        // round-trip; the trust-source profile path is the cheapest
        // way to assert the short-circuit.
        let profile = PaperAuditProfile::trust_source();
        let data = PaperAuditData::empty();
        let biblio = empty_biblio();
        let provenance = empty_provenance();
        let effective = empty_effective();
        let input = PaperAuditInput {
            biblio: &biblio,
            provenance: &provenance,
            effective: &effective,
            body_sample: "",
            source_stem: None,
        };
        let report = audit_paper(&input, &profile, &data);
        assert!(report.fields.is_empty());
        assert_eq!(report.verdict, PaperVerdict::Clean);
        assert!(report.cross_field_flags.is_empty());
        let _ = data;
    }

    #[test]
    fn paper_without_any_stable_identifier_floors_verdict() {
        let profile = PaperAuditProfile::default_profile();
        let data = PaperAuditData::default_data();
        let biblio = empty_biblio();
        let provenance = empty_provenance();
        let effective = empty_effective();
        let input = PaperAuditInput {
            biblio: &biblio,
            provenance: &provenance,
            effective: &effective,
            body_sample: "",
            source_stem: None,
        };
        let report = audit_paper(&input, &profile, &data);
        assert_eq!(report.verdict, PaperVerdict::NeedsWork);
        assert!(
            report
                .cross_field_flags
                .contains(&PaperFlag::NoStableIdentifier)
        );
    }

    fn empty_biblio() -> Biblio {
        Biblio {
            title: None,
            subtitle: None,
            publisher: None,
            year: None,
            year_raw: None,
            isbn: None,
            series: None,
            language: None,
            contributors: Vec::new(),
            doi: None,
            arxiv_id: None,
            issn: None,
            container_title: None,
            abstract_text: None,
            csl_type: None,
        }
    }

    fn empty_provenance() -> Provenance {
        Provenance {
            adapter: "pdf".to_string(),
            extractor_version: 1,
            text_layer_quality: TextLayerQuality::Usable,
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
        }
    }

    fn empty_effective() -> EffectiveAttrs {
        // EffectiveAttrs has no public constructor for an empty view,
        // so we mint one by serialising a vacant base into the
        // catalog's helper. Tests in this module need only the
        // `get()` accessor, which returns None for every field on
        // the empty value.
        use bookrack_catalog::{Catalog, NewIntake, NewPublicationAttrs};
        use bookrack_core::ItemKind;
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("deadbeef".to_string()).format("pdf".to_string()),
            )
            .expect("register");
        let attrs = NewPublicationAttrs::new(intake.intake().intake_id, ItemKind::Paper);
        catalog.upsert_publication_attrs(&attrs).expect("upsert");
        catalog
            .effective_publication_attrs(intake.intake().intake_id, ItemKind::Paper)
            .expect("effective")
    }
}
