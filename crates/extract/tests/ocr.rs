// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the OCR adapter, driven by the synthetic
//! single-file Markdown product under
//! `tests/fixtures/ocr/v1/sample.bookrack-ocr.v1.md` and its source PDF
//! at `tests/fixtures/ocr/v1/sample.pdf`.
//!
//! The OCR adapter does not need PDFium to parse the markdown body; the
//! native library is consulted only for the `/Outline` and `/Info` lift
//! when a source PDF is provided. A contributor without PDFium still
//! gets coverage of the markdown half (see
//! [`ocr_adapter_extracts_pages_without_source_pdf`]); the source-PDF
//! coupling test gates on [`common::pdfium_available`] and skips
//! cleanly on a machine without the binary.
//!
//! Assertions are structural — block counts, source-unit indices,
//! provenance fields, the single outline label — matching the PDF
//! adapter's "no byte-exact golden output" discipline.

mod common;

use std::path::{Path, PathBuf};

use bookrack_extract::{
    BlockKind, ContributorRole, Extraction, OCR_INTAKE_VERSION, TextLayerQuality, ocr,
};
use common::pdfium_available;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ocr/v1")
        .join(name)
}

#[test]
fn ocr_adapter_extracts_pages_without_source_pdf() {
    let ex = ocr::extract(&fixture("sample.bookrack-ocr.v1.md"), None, None).expect("extract");
    assert_provenance_shape(&ex, None);

    // No source PDF → TOC and biblio default to empty.
    assert!(
        ex.toc.entries.is_empty(),
        "toc must be empty without a source PDF, got {:?}",
        ex.toc.entries
    );
    assert_eq!(ex.biblio.title, None);
    assert_eq!(ex.biblio.contributors.len(), 0);
    assert_eq!(ex.biblio.year, None);

    // 3 markers → at least one Body block per sheet, source_unit = sheet - 1.
    let body_sheets: std::collections::BTreeSet<u32> = ex
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Body))
        .map(|b| b.source_unit)
        .collect();
    assert_eq!(body_sheets, [0, 1, 2].into_iter().collect());

    // No headings, footnotes, captions, or other classifications: OCR
    // only emits body blocks in this MVP.
    for block in &ex.blocks {
        assert!(
            matches!(block.kind, BlockKind::Body),
            "non-body block leaked: {block:?}",
        );
    }
}

#[test]
fn ocr_adapter_lifts_outline_and_info_from_source_pdf() {
    if !pdfium_available() {
        return;
    }
    let ex = ocr::extract(
        &fixture("sample.bookrack-ocr.v1.md"),
        Some(&fixture("sample.pdf")),
        Some("test-sha-of-source-pdf"),
    )
    .expect("extract");
    assert_provenance_shape(&ex, Some("test-sha-of-source-pdf"));

    // /Outline lifted: the single heading entry sits on the PDF's
    // second physical sheet (0-based page index 1), which is the
    // chapter page in the typst-rendered fixture.
    assert_eq!(
        ex.toc.entries.len(),
        1,
        "expected one outline entry, got {:?}",
        ex.toc.entries
    );
    let entry = &ex.toc.entries[0];
    assert_eq!(entry.label, "Chapter One");
    let anchored = entry.start_block.expect("outline entry anchors to a block");
    assert_eq!(
        ex.blocks[anchored].source_unit, 1,
        "the outline must anchor to the sheet-2 (0-based 1) page"
    );

    // /Info lifted: title, author (one contributor with author role),
    // and the year parsed from the CreationDate.
    assert_eq!(ex.biblio.title.as_deref(), Some("Synthetic OCR Fixture"));
    assert_eq!(ex.biblio.contributors.len(), 1);
    assert_eq!(ex.biblio.contributors[0].name, "Bookrack Tests");
    assert!(matches!(
        ex.biblio.contributors[0].role,
        ContributorRole::Author
    ));
    assert_eq!(ex.biblio.year, Some(2024));
}

/// Shared provenance checks: the values that must hold whether or not
/// the source PDF was supplied.
fn assert_provenance_shape(ex: &Extraction, expected_sha: Option<&str>) {
    assert_eq!(ex.provenance.adapter, "ocr-pages");
    assert_eq!(ex.provenance.extractor_version, OCR_INTAKE_VERSION);
    assert!(
        matches!(ex.provenance.text_layer_quality, TextLayerQuality::Doubtful),
        "OCR-intake stamps Doubtful, got {:?}",
        ex.provenance.text_layer_quality,
    );
    assert!(
        ex.provenance.skipped_units.is_empty(),
        "MVP records no skipped units",
    );
    assert_eq!(
        ex.provenance.derived_from_sha256.as_deref(),
        expected_sha,
        "derived_from_sha256 must echo the caller's value",
    );
    assert_eq!(
        ex.provenance.partial_pages, None,
        "complete (non-partial) ingest leaves partial_pages as None",
    );
}
