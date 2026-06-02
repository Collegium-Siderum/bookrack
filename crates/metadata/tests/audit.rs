//! Table-driven tests for the metadata audit.
//!
//! Each test seeds a small in-memory catalog with one node's base
//! attributes, builds the [`bookrack_extract`] inputs as bare
//! synthetic values, runs the audit, and asserts on the
//! per-field grade and the flag set.

use bookrack_catalog::{Catalog, EffectiveAttrs, NewPublicationAttrs};
use bookrack_extract::{Biblio, Provenance, TextLayerQuality};
use bookrack_metadata::{
    AuditInput, Confidence, FieldGrade, FieldReport, Flag, MetadataReport, TocStats, Verdict, audit,
};

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
        extractor_version: "test-1".to_string(),
        text_layer_quality: quality,
        skipped_units: Vec::new(),
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    // "placeholder epub watermark example" — pirate brand observed verbatim
    // in real EPUB metadata.
    let watermark = "epub\u{7532}\u{4E59}\u{4E19}\u{4E01}";
    seed_base(
        &catalog,
        Some("A Book"),
        Some(watermark),
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
    };
    let report = audit(&input);
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
