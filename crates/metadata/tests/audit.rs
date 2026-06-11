//! Table-driven tests for the metadata audit.
//!
//! Each test seeds a small in-memory catalog with one node's base
//! attributes, builds the [`bookrack_extract`] inputs as bare
//! synthetic values, runs the audit, and asserts on the
//! per-field grade and the flag set.

use bookrack_catalog::{Catalog, EffectiveAttrs, NewPublicationAttrs};
use bookrack_extract::{Biblio, Provenance, TextLayerQuality};
use bookrack_metadata::{
    AuditData, AuditInput, AuditProfile, Confidence, FieldGrade, FieldOrigins, FieldReport, Flag,
    MetadataReport, TocStats, Verdict, audit,
};

/// Shared data set the audit tests use: starts from the shipped
/// default (so URL / abbreviation / placeholder / extension defaults
/// stay in scope) and adds the whitelist entry the
/// "Oxford University Press" cases depend on, a synthetic CJK
/// watermark token from a fixture, and the CJK volume-suffix tokens
/// the bracketed-title classifier exercises.
fn test_data() -> &'static AuditData {
    use std::sync::OnceLock;
    static DATA: OnceLock<AuditData> = OnceLock::new();
    DATA.get_or_init(|| {
        let mut data = AuditData::default_data();
        data.publisher_whitelist = vec!["Oxford University Press".to_string()];
        // A synthetic CJK token loaded from a fixture. Exercises the
        // substring path in `watermark_cjk_tokens`. Real brand strings
        // live in the operator's `audit_data.toml`, never in source.
        data.watermark_cjk_tokens = vec![
            include_str!("fixtures/watermarks/synthetic_cjk_token.txt")
                .trim()
                .to_string(),
        ];
        // CJK volume / edition / printing suffixes recognised by the
        // bracketed-title classifier: `\u{518C}` / `\u{7248}` /
        // `\u{672C}`. In production these come from the operator's
        // `audit_data.toml`.
        data.volume_suffix_tokens = vec![
            "\u{518C}".to_string(),
            "\u{7248}".to_string(),
            "\u{672C}".to_string(),
        ];
        data
    })
}

const INTAKE: i64 = 1;
const SCOPE: &str = "book";

/// Seed a node's base attributes from the spelled-out arguments.
#[allow(clippy::too_many_arguments)]
fn seed_base(
    catalog: &Catalog,
    title: Option<&str>,
    publisher: Option<&str>,
    year: Option<&str>,
    isbn: Option<&str>,
    language: Option<&str>,
    series: Option<&str>,
    subtitle: Option<&str>,
) {
    let mut attrs = NewPublicationAttrs::new(INTAKE, SCOPE);
    attrs.title = title.map(str::to_string);
    attrs.publisher = publisher.map(str::to_string);
    attrs.year = year.map(str::to_string);
    attrs.isbn = isbn.map(str::to_string);
    attrs.language = language.map(str::to_string);
    attrs.series = series.map(str::to_string);
    attrs.subtitle = subtitle.map(str::to_string);
    catalog.upsert_publication_attrs(&attrs).expect("base");
}

fn provenance(adapter: &str, quality: TextLayerQuality) -> Provenance {
    Provenance {
        adapter: adapter.to_string(),
        extractor_version: 1,
        text_layer_quality: quality,
        skipped_units: Vec::new(),
        derived_from_sha256: None,
        partial_pages: None,
    }
}

fn biblio() -> Biblio {
    Biblio::default()
}

fn toc_stats() -> TocStats {
    TocStats::default()
}

fn effective_of(catalog: &Catalog) -> EffectiveAttrs {
    catalog
        .effective_publication_attrs(INTAKE, SCOPE)
        .expect("effective")
}

fn field<'a>(report: &'a MetadataReport, field: &str) -> &'a FieldReport {
    report
        .fields
        .iter()
        .find(|f| f.field == field)
        .unwrap_or_else(|| panic!("no field {field} in report"))
}

#[test]
fn epub_with_complete_record_grades_clean_and_high() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Oxford University Press"),
        Some("2005"),
        Some("978-3-16-148410-0"),
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert_eq!(report.verdict, Verdict::Clean);
    assert_eq!(report.confidence, Confidence::High);
    assert_eq!(field(&report, "title").grade, FieldGrade::Strong);
    assert_eq!(field(&report, "language").grade, FieldGrade::Strong);
    assert_eq!(field(&report, "publisher").grade, FieldGrade::Strong);
    assert!(
        field(&report, "publisher")
            .flags
            .contains(&Flag::PublisherWhitelisted)
    );
}

#[test]
fn epub_year_from_timestamp_shaped_dc_date_is_downgraded() {
    // A real-world EPUB shape: `dc:date` carries a build/export
    // timestamp. The extractor parses out 2011 as the year, but the
    // raw string ends with `T16:00:00+00:00` — a strong hint that this
    // is the production date, not the publication year. The audit must
    // weaken the year grade and raise `DateLooksLikeTimestamp`.
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Oxford University Press"),
        Some("2011"),
        Some("978-3-16-148410-0"),
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = Biblio {
        year: Some(2011),
        year_raw: Some("2011-09-29T16:00:00+00:00".to_string()),
        ..Biblio::default()
    };
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert_eq!(field(&report, "year").grade, FieldGrade::Medium);
    assert!(
        field(&report, "year")
            .flags
            .contains(&Flag::DateLooksLikeTimestamp)
    );
}

#[test]
fn epub_year_from_a_plain_year_string_stays_strong() {
    // The dc:date carried a bare year — no time component, so the
    // timestamp signal must not fire.
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Oxford University Press"),
        Some("2011"),
        Some("978-3-16-148410-0"),
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = Biblio {
        year: Some(2011),
        year_raw: Some("2011".to_string()),
        ..Biblio::default()
    };
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert_eq!(field(&report, "year").grade, FieldGrade::Strong);
    assert!(
        !field(&report, "year")
            .flags
            .contains(&Flag::DateLooksLikeTimestamp)
    );
}

#[test]
fn user_override_year_skips_the_timestamp_shape_signal() {
    // The raw biblio carries a timestamp-shaped date, but the user
    // overrode the year. The override wins and the timestamp signal
    // must not fire — the year value no longer came from the file.
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Oxford University Press"),
        Some("2011"),
        Some("978-3-16-148410-0"),
        Some("en"),
        None,
        None,
    );
    catalog
        .set_override(&bookrack_catalog::NewOverride::new(
            INTAKE,
            SCOPE,
            "year",
            Some("1990".to_string()),
            "human",
        ))
        .expect("override");
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = Biblio {
        year: Some(2011),
        year_raw: Some("2011-09-29T16:00:00+00:00".to_string()),
        ..Biblio::default()
    };
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert_eq!(field(&report, "year").grade, FieldGrade::Strong);
    assert!(
        !field(&report, "year")
            .flags
            .contains(&Flag::DateLooksLikeTimestamp)
    );
}

#[test]
fn empty_record_grades_needs_work_and_low() {
    let catalog = Catalog::open_in_memory().expect("open");
    // No base row at all.
    let effective = effective_of(&catalog);
    let prov = provenance("txt", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert_eq!(report.verdict, Verdict::NeedsWork);
    assert_eq!(report.confidence, Confidence::Low);
    assert_eq!(field(&report, "title").grade, FieldGrade::Missing);
    assert_eq!(field(&report, "language").grade, FieldGrade::Missing);
    assert!(field(&report, "title").flags.contains(&Flag::Empty));
}

#[test]
fn title_equal_to_filename_is_flagged() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("the-source-stem"),
        None,
        None,
        None,
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Sample English body text.",
        total_blocks: 10,
        source_stem: Some("the-source-stem"),
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        field(&report, "title")
            .flags
            .contains(&Flag::EqualsFilename)
    );
    assert_eq!(field(&report, "title").grade, FieldGrade::Medium);
}

#[test]
fn placeholder_title_is_flagged() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("Unknown"),
        None,
        None,
        None,
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Some English body.",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        field(&report, "title")
            .flags
            .contains(&Flag::PlaceholderValue)
    );
    assert!(field(&report, "title").grade != FieldGrade::Strong);
}

#[test]
fn invalid_isbn_checksum_is_flagged() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        None,
        None,
        // 13-digit but bad checksum.
        Some("978-3-16-148410-1"),
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Some English body.",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        field(&report, "isbn")
            .flags
            .contains(&Flag::IsbnCheckFailed)
    );
}

#[test]
fn year_outside_range_is_flagged() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        None,
        Some("0101"),
        None,
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Some English body.",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(field(&report, "year").flags.contains(&Flag::YearOutOfRange));
}

#[test]
fn pdf_year_is_flagged_as_likely_file_date() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        None,
        Some("2018"),
        None,
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("pdf", TextLayerQuality::Usable);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Some English body.",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        field(&report, "year")
            .flags
            .contains(&Flag::PdfYearLikelyFileDate)
    );
}

#[test]
fn watermark_publisher_is_flagged() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        Some("free download www.example.net"),
        None,
        None,
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Some English body.",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        field(&report, "publisher")
            .flags
            .contains(&Flag::SourceWatermark)
    );
    assert!(matches!(
        field(&report, "publisher").grade,
        FieldGrade::Weak | FieldGrade::Missing
    ));
}

#[test]
fn cjk_watermark_publisher_is_flagged() {
    let catalog = Catalog::open_in_memory().expect("open");
    // ASCII prefix followed by the synthetic CJK token from
    // `test_rules().watermark_cjk_tokens` — exercises the substring
    // match without naming any real distribution brand.
    let token = include_str!("fixtures/watermarks/synthetic_cjk_token.txt").trim();
    let watermark = format!("epub{token}");
    seed_base(
        &catalog,
        Some("A Book"),
        Some(&watermark),
        None,
        None,
        Some("zh"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Sample body.",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        field(&report, "publisher")
            .flags
            .contains(&Flag::SourceWatermark)
    );
    assert!(matches!(
        field(&report, "publisher").grade,
        FieldGrade::Weak | FieldGrade::Missing
    ));
}

#[test]
fn doubtful_text_layer_downgrades_present_fields() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        None,
        None,
        None,
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::Doubtful);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Some English body.",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        field(&report, "title")
            .flags
            .contains(&Flag::DoubtfulTextLayer)
    );
    assert!(
        field(&report, "language")
            .flags
            .contains(&Flag::DoubtfulTextLayer)
    );
}

#[test]
fn language_disagreeing_with_body_is_flagged() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        None,
        None,
        None,
        Some("zh"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let body = "Mostly English text with no CJK characters present at all.";
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: body,
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        field(&report, "language")
            .flags
            .contains(&Flag::LangMismatchesBody)
    );
}

#[test]
fn cjk_body_agrees_with_zh_language() {
    // CJK fixture: a small Chinese sentence held under tests/fixtures.
    let body = include_str!("fixtures/cjk_sample.txt");
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        None,
        None,
        None,
        Some("zh-Hans"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: body,
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(
        !field(&report, "language")
            .flags
            .contains(&Flag::LangMismatchesBody)
    );
}

#[test]
fn non_bcp47_language_is_flagged() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        None,
        None,
        None,
        Some("English"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Some English body.",
        total_blocks: 10,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(field(&report, "language").flags.contains(&Flag::NonBcp47));
}

#[test]
fn copyright_blocks_are_the_leading_indices() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Book"),
        None,
        None,
        None,
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "Some English body.",
        total_blocks: 3,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert_eq!(report.copyright_blocks, vec![0, 1, 2]);
}

#[test]
fn verdict_tokens_and_confidence_strings_round_trip() {
    assert_eq!(Verdict::Clean.as_token(), "clean");
    assert_eq!(Verdict::NeedsWork.as_token(), "needs_work");
    assert_eq!(Confidence::High.as_str(), "high");
    assert_eq!(Confidence::Medium.as_str(), "medium");
    assert_eq!(Confidence::Low.as_str(), "low");
}

/// Build a [`Catalog`] seeded with a complete-record book that grades
/// `Clean` + `High` at the per-field rollup — the baseline against
/// which TOC shape signals are checked.
fn clean_high_catalog() -> Catalog {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Oxford University Press"),
        Some("2005"),
        Some("978-3-16-148410-0"),
        Some("en"),
        None,
        None,
    );
    catalog
}

#[test]
fn audit_toc_shape_clean_yields_no_flags() {
    let catalog = clean_high_catalog();
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 50,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(report.shape_flags.is_empty());
    assert_eq!(report.verdict, Verdict::Clean);
    assert_eq!(report.confidence, Confidence::High);
}

#[test]
fn audit_toc_shape_severe_pulls_verdict_and_confidence_down() {
    let catalog = clean_high_catalog();
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = TocStats {
        total_toc_entries: 10,
        unanchored_toc_entries: 6,
        suspicious_flat: false,
        heading_block_skew: false,
    };
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 200,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert!(report.shape_flags.contains(&Flag::TocUnanchoredSome));
    assert!(report.shape_flags.contains(&Flag::TocUnanchoredHalf));
    assert_eq!(report.verdict, Verdict::NeedsWork);
    assert_eq!(report.confidence, Confidence::Low);
}

#[test]
fn audit_toc_shape_mild_caps_confidence_at_medium() {
    let catalog = clean_high_catalog();
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    // suspicious_flat with only 6 entries stays under the severe
    // promotion threshold; mild caps confidence at Medium without
    // moving the verdict.
    let stats = TocStats {
        total_toc_entries: 6,
        unanchored_toc_entries: 0,
        suspicious_flat: true,
        heading_block_skew: false,
    };
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 80,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &AuditProfile::default());
    assert_eq!(report.shape_flags, vec![Flag::TocSuspiciousFlat]);
    assert_eq!(report.verdict, Verdict::Clean);
    assert_eq!(report.confidence, Confidence::Medium);
}

#[test]
fn audit_toc_shape_never_strengthens() {
    // Table-driven direction invariant. Start from the all-empty
    // record's `NeedsWork` + `Low` baseline and walk every 5-bit
    // combination of the inputs `audit_toc_shape` reads. No
    // combination is allowed to push the verdict back to `Clean` or
    // the confidence above `Low`.
    let catalog = Catalog::open_in_memory().expect("open");
    let effective = effective_of(&catalog);
    let prov = provenance("txt", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let baseline = {
        let stats = TocStats::default();
        let input = AuditInput {
            biblio: &biblio,
            provenance: &prov,
            effective: &effective,
            toc_stats: &stats,
            body_sample: "",
            total_blocks: 10,
            source_stem: None,
            data: test_data(),
            origins: FieldOrigins::empty(),
        };
        audit(&input, &AuditProfile::default())
    };
    assert_eq!(baseline.verdict, Verdict::NeedsWork);
    assert_eq!(baseline.confidence, Confidence::Low);

    for mask in 0u8..32 {
        let empty_large = mask & 0b00001 != 0;
        let unanchored_some = mask & 0b00010 != 0;
        let unanchored_half = mask & 0b00100 != 0;
        let suspicious_flat = mask & 0b01000 != 0;
        let heading_skew = mask & 0b10000 != 0;
        let total = if unanchored_half { 10 } else { 4 };
        let unanchored = if unanchored_half {
            6
        } else if unanchored_some {
            1
        } else {
            0
        };
        let stats = TocStats {
            total_toc_entries: total,
            unanchored_toc_entries: unanchored,
            suspicious_flat,
            heading_block_skew: heading_skew,
        };
        let blocks = if empty_large { 200 } else { 10 };
        let stats_with_empty = if empty_large {
            TocStats {
                total_toc_entries: 0,
                unanchored_toc_entries: 0,
                suspicious_flat,
                heading_block_skew: heading_skew,
            }
        } else {
            stats
        };
        let input = AuditInput {
            biblio: &biblio,
            provenance: &prov,
            effective: &effective,
            toc_stats: &stats_with_empty,
            body_sample: "",
            total_blocks: blocks,
            source_stem: None,
            data: test_data(),
            origins: FieldOrigins::empty(),
        };
        let report = audit(&input, &AuditProfile::default());
        assert_eq!(
            report.verdict,
            Verdict::NeedsWork,
            "mask {mask:05b} flipped verdict back to Clean"
        );
        assert_eq!(
            report.confidence,
            Confidence::Low,
            "mask {mask:05b} pushed confidence above Low"
        );
    }
}

#[test]
fn audit_input_carries_no_review_status_field() {
    // Compile-time guard: destructuring `AuditInput` exhaustively below
    // breaks the build if a future contributor adds any field — a
    // review-status field in particular. The audit must stay a pure
    // function of extract plus catalog base/override state; the
    // review status channel is strictly outside its surface (see
    // `crates/catalog/src/node_reviews.rs:8-15`).
    let catalog = Catalog::open_in_memory().expect("open");
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "",
        total_blocks: 0,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let AuditInput {
        biblio: _,
        provenance: _,
        effective: _,
        toc_stats: _,
        body_sample: _,
        total_blocks: _,
        source_stem: _,
        data: _,
        origins: _,
    } = input;
}

/// Build a stock `AuditInput` against an in-memory catalog seeded with
/// the spelled-out fields. Helper used by the toggle-off tests below.
fn run_with(
    profile: &AuditProfile,
    title: Option<&str>,
    publisher: Option<&str>,
    year: Option<&str>,
    language: Option<&str>,
    body_sample: &str,
    adapter: &str,
) -> MetadataReport {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(&catalog, title, publisher, year, None, language, None, None);
    let effective = effective_of(&catalog);
    let prov = provenance(adapter, TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample,
        total_blocks: 0,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    audit(&input, profile)
}

#[test]
fn toggle_off_year_range_check_suppresses_out_of_range_flag() {
    let mut profile = AuditProfile::default();
    profile.year.range_check = false;
    let report = run_with(
        &profile,
        Some("T"),
        None,
        Some("9999"),
        Some("en"),
        "",
        "epub",
    );
    let year = field(&report, "year");
    assert!(!year.flags.contains(&Flag::YearOutOfRange));
}

#[test]
fn toggle_off_pdf_year_likely_file_date_suppresses_flag() {
    let mut profile = AuditProfile::default();
    profile.year.pdf_likely_file_date = false;
    let report = run_with(
        &profile,
        Some("T"),
        None,
        Some("2005"),
        Some("en"),
        "",
        "pdf",
    );
    let year = field(&report, "year");
    assert!(!year.flags.contains(&Flag::PdfYearLikelyFileDate));
}

#[test]
fn toggle_off_language_bcp47_check_suppresses_flag() {
    let mut profile = AuditProfile::default();
    profile.language.bcp47_check = false;
    let report = run_with(&profile, Some("T"), None, None, Some("english"), "", "epub");
    let lang = field(&report, "language");
    assert!(!lang.flags.contains(&Flag::NonBcp47));
}

#[test]
fn toggle_off_publisher_url_watermark_suppresses_flag() {
    let mut profile = AuditProfile::default();
    profile.publisher.url_watermark = false;
    let report = run_with(
        &profile,
        Some("T"),
        Some("https://example.com/free-ebooks"),
        None,
        Some("en"),
        "",
        "epub",
    );
    let publisher = field(&report, "publisher");
    assert!(!publisher.flags.contains(&Flag::SourceWatermark));
}

#[test]
fn toggle_off_source_prior_keeps_pdf_field_strong() {
    let mut profile = AuditProfile::default();
    profile.source_prior.enabled = false;
    profile.year.pdf_likely_file_date = false;
    let report = run_with(
        &profile,
        Some("Plausible Title"),
        Some("Sample Press"),
        Some("2010"),
        Some("en"),
        "The quick brown fox jumps over the lazy dog.",
        "pdf",
    );
    assert_eq!(report.confidence, Confidence::High);
    for f in &report.fields {
        assert!(
            !f.flags.contains(&Flag::SourcePriorWeak),
            "source_prior disabled but {} still carries the flag",
            f.field
        );
    }
}

#[test]
fn toggle_off_copyright_blocks_yields_empty_window() {
    let mut profile = AuditProfile::default();
    profile.copyright_blocks.enabled = false;
    let report = run_with(
        &profile,
        Some("Plausible Title"),
        Some("Sample Press"),
        Some("2010"),
        Some("en"),
        "",
        "epub",
    );
    assert!(report.copyright_blocks.is_empty());
}

#[test]
fn toggle_off_title_bracketed_suppresses_flag() {
    let mut profile = AuditProfile::default();
    profile.title.series_paren = false;
    profile.title.marketing_block = false;
    profile.title.aggregator_marker = false;
    profile.title.volume_marker = false;
    let title_with_brackets = "A Real Title (Translated Series Marker)";
    let report = run_with(
        &profile,
        Some(title_with_brackets),
        None,
        None,
        Some("en"),
        "",
        "epub",
    );
    let title = field(&report, "title");
    assert!(!title.flags.contains(&Flag::TitleSeriesParen));
    assert!(!title.flags.contains(&Flag::TitleMarketingBlock));
    assert!(!title.flags.contains(&Flag::TitleAggregatorMarker));
    assert!(!title.flags.contains(&Flag::TitleVolumeMarker));
}

#[test]
fn title_with_trailing_series_paren_raises_series_flag() {
    let profile = AuditProfile::default();
    let title = "A Real Title (Translated Series Marker)";
    let report = run_with(&profile, Some(title), None, None, Some("en"), "", "epub");
    let f = field(&report, "title");
    assert!(f.flags.contains(&Flag::TitleSeriesParen));
    assert!(!f.flags.contains(&Flag::TitleVolumeMarker));
}

#[test]
fn title_with_lenticular_tail_raises_marketing_flag() {
    let profile = AuditProfile::default();
    // CJK title with lenticular brackets at the tail.
    let title = include_str!("fixtures/titles/lenticular_marketing.txt").trim();
    let report = run_with(&profile, Some(title), None, None, Some("zh"), "", "epub");
    let f = field(&report, "title");
    assert!(f.flags.contains(&Flag::TitleMarketingBlock));
}

#[test]
fn title_with_leading_square_brackets_raises_aggregator_flag() {
    let profile = AuditProfile::default();
    // CJK title with square brackets at the head.
    let title = include_str!("fixtures/titles/square_aggregator.txt").trim();
    let report = run_with(&profile, Some(title), None, None, Some("zh"), "", "epub");
    let f = field(&report, "title");
    assert!(f.flags.contains(&Flag::TitleAggregatorMarker));
}

#[test]
fn title_with_volume_marker_raises_volume_flag_without_weakening() {
    let profile = AuditProfile::default();
    // Trailing volume marker: fullwidth parens around a CJK volume
    // suffix.
    let title = include_str!("fixtures/titles/fullwidth_volume.txt").trim();
    let report = run_with(&profile, Some(title), None, None, Some("zh"), "", "epub");
    let f = field(&report, "title");
    assert!(f.flags.contains(&Flag::TitleVolumeMarker));
    // Volume markers must not weaken the title grade.
    assert_eq!(f.grade, FieldGrade::Strong);
}

#[test]
fn toggle_off_toc_shape_suppresses_empty_large_body_flag() {
    let mut profile = AuditProfile::default();
    profile.toc_shape.empty_large_body = false;
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("T"),
        None,
        Some("2010"),
        None,
        Some("en"),
        None,
        None,
    );
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = TocStats::default();
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "",
        // Above the default large_body_min_blocks (100); without the
        // toggle disabled this would emit Flag::TocEmptyLargeBody.
        total_blocks: 500,
        source_stem: None,
        data: test_data(),
        origins: FieldOrigins::empty(),
    };
    let report = audit(&input, &profile);
    assert!(!report.shape_flags.contains(&Flag::TocEmptyLargeBody));
}

#[test]
fn override_is_exempt_from_the_source_prior() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Wrong Press"),
        Some("2005"),
        None,
        Some("en"),
        None,
        None,
    );
    catalog
        .set_override(&bookrack_catalog::NewOverride::new(
            INTAKE,
            SCOPE,
            "publisher",
            Some("Verified Press".to_string()),
            "human",
        ))
        .expect("override");
    let effective = effective_of(&catalog);
    let prov = provenance("pdf", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let mut origins = FieldOrigins::empty();
    origins.add_override("publisher", false);
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins,
    };
    let report = audit(&input, &AuditProfile::default());
    // The curated publisher escapes the weak PDF prior; the extracted
    // title does not.
    let publisher = field(&report, "publisher");
    assert!(!publisher.flags.contains(&Flag::SourcePriorWeak));
    assert_eq!(publisher.grade, FieldGrade::Strong);
    let title = field(&report, "title");
    assert!(title.flags.contains(&Flag::SourcePriorWeak));
}

#[test]
fn override_is_exempt_from_the_doubtful_text_layer() {
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Wrong Press"),
        None,
        None,
        None,
        None,
        None,
    );
    catalog
        .set_override(&bookrack_catalog::NewOverride::new(
            INTAKE,
            SCOPE,
            "publisher",
            Some("Verified Press".to_string()),
            "human",
        ))
        .expect("override");
    let effective = effective_of(&catalog);
    let prov = provenance("txt", TextLayerQuality::Doubtful);
    let biblio = biblio();
    let stats = toc_stats();
    let mut origins = FieldOrigins::empty();
    origins.add_override("publisher", false);
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: None,
        data: test_data(),
        origins,
    };
    let report = audit(&input, &AuditProfile::default());
    let publisher = field(&report, "publisher");
    assert!(!publisher.flags.contains(&Flag::DoubtfulTextLayer));
    assert!(!publisher.flags.contains(&Flag::SourcePriorWeak));
    let title = field(&report, "title");
    assert!(title.flags.contains(&Flag::DoubtfulTextLayer));
    assert!(title.flags.contains(&Flag::SourcePriorWeak));
}

#[test]
fn confirmed_override_pins_the_grade_despite_heuristics() {
    // The curated title happens to equal the source filename. The
    // heuristic stays on the report for observability, but a confirmed
    // override is graded on the curator's signature, not on suspicion.
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("Wrong Title"),
        Some("Oxford University Press"),
        Some("2005"),
        Some("978-3-16-148410-0"),
        Some("en"),
        None,
        None,
    );
    catalog
        .set_override(&bookrack_catalog::NewOverride::new(
            INTAKE,
            SCOPE,
            "title",
            Some("My Title".to_string()),
            "human",
        ))
        .expect("override");
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let mut origins = FieldOrigins::empty();
    origins.add_override("title", true);
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: Some("My Title"),
        data: test_data(),
        origins,
    };
    let report = audit(&input, &AuditProfile::default());
    let title = field(&report, "title");
    assert!(title.flags.contains(&Flag::EqualsFilename));
    assert_eq!(title.grade, FieldGrade::Strong);
}

#[test]
fn confirmed_override_keeps_validation_failures() {
    // Confirmation silences extraction suspicion, not arithmetic: a
    // confirmed ISBN with a bad checksum stays weakened and flagged.
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Oxford University Press"),
        Some("2005"),
        None,
        Some("en"),
        None,
        None,
    );
    catalog
        .set_override(&bookrack_catalog::NewOverride::new(
            INTAKE,
            SCOPE,
            "isbn",
            Some("978-3-16-148410-1".to_string()),
            "human",
        ))
        .expect("override");
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let mut origins = FieldOrigins::empty();
    origins.add_override("isbn", true);
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins,
    };
    let report = audit(&input, &AuditProfile::default());
    let isbn = field(&report, "isbn");
    assert!(isbn.flags.contains(&Flag::IsbnCheckFailed));
    assert_ne!(isbn.grade, FieldGrade::Strong);
}

#[test]
fn voided_should_field_reads_as_a_neutral_gap() {
    // A voided publisher is a deliberate, recorded gap: medium grade,
    // a single Voided flag, and the rollup caps at Medium instead of
    // collapsing to Low the way a missing extraction would.
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("A Test Book"),
        Some("Pirate Site Press"),
        Some("2005"),
        Some("978-3-16-148410-0"),
        Some("en"),
        None,
        None,
    );
    catalog
        .set_override(&bookrack_catalog::NewOverride::new(
            INTAKE,
            SCOPE,
            "publisher",
            None,
            "human",
        ))
        .expect("void");
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let mut origins = FieldOrigins::empty();
    origins.add_voided("publisher");
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: Some("a-test-book"),
        data: test_data(),
        origins,
    };
    let report = audit(&input, &AuditProfile::default());
    let publisher = field(&report, "publisher");
    assert_eq!(publisher.grade, FieldGrade::Medium);
    assert_eq!(publisher.flags, vec![Flag::Voided]);
    assert_eq!(report.verdict, Verdict::Clean);
    assert_eq!(report.confidence, Confidence::Medium);
}

#[test]
fn voided_required_field_still_needs_work() {
    // Voiding a required field records the judgement but cannot excuse
    // the gap: a book without a title still needs work.
    let catalog = Catalog::open_in_memory().expect("open");
    seed_base(
        &catalog,
        Some("Garbage Title"),
        Some("Oxford University Press"),
        Some("2005"),
        Some("978-3-16-148410-0"),
        Some("en"),
        None,
        None,
    );
    catalog
        .set_override(&bookrack_catalog::NewOverride::new(
            INTAKE, SCOPE, "title", None, "human",
        ))
        .expect("void");
    let effective = effective_of(&catalog);
    let prov = provenance("epub", TextLayerQuality::BornDigital);
    let biblio = biblio();
    let stats = toc_stats();
    let mut origins = FieldOrigins::empty();
    origins.add_voided("title");
    let input = AuditInput {
        biblio: &biblio,
        provenance: &prov,
        effective: &effective,
        toc_stats: &stats,
        body_sample: "The quick brown fox jumps over the lazy dog.",
        total_blocks: 100,
        source_stem: None,
        data: test_data(),
        origins,
    };
    let report = audit(&input, &AuditProfile::default());
    let title = field(&report, "title");
    assert_eq!(title.grade, FieldGrade::Missing);
    assert_eq!(title.flags, vec![Flag::Voided]);
    assert_eq!(report.verdict, Verdict::NeedsWork);
    assert_eq!(report.confidence, Confidence::Low);
}
