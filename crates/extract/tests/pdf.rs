// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the PDF adapter, driven by synthetic fixtures
//! under `tests/fixtures/pdf/`.
//!
//! The PDF adapter needs the PDFium native library; a contributor
//! without it set up sees these tests skip rather than fail (see
//! [`common::pdfium_available`]). CI always provides the binary.
//!
//! Assertions are structural — block counts, quality grades, known
//! phrases — not byte-exact golden output: PDFium is a native
//! dependency whose exact text spacing can shift between builds, and a
//! brittle golden would break on an unrelated bump.

mod common;

use std::collections::BTreeSet;

use bookrack_extract::{
    BlockKind, ContributorRole, ExtractError, ExtractOutcome, Extraction, TextLayerQuality, extract,
};
use common::{pdf_fixture, pdfium_available};

/// Every block's text, joined — for substring assertions that do not
/// care which block a phrase landed in.
fn joined_text(extraction: &Extraction) -> String {
    extraction
        .blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract a fixture, asserting it produced a usable text layer.
fn extracted(name: &str) -> Extraction {
    match extract(&pdf_fixture(name)) {
        Ok(ExtractOutcome::Extracted(e)) => e,
        other => panic!("{name}: expected a usable text layer, got {other:?}"),
    }
}

#[test]
fn clean_pdfs_extract_a_usable_text_layer() {
    if !pdfium_available() {
        return;
    }
    // Every born-digital fixture must extract, grade as a kept layer,
    // produce body blocks, and skip no pages.
    for name in [
        "prose_en.pdf",
        "prose_cjk.pdf",
        "two_column.pdf",
        "toc_deep.pdf",
        "biblio_garbage.pdf",
        "encrypted_restricted.pdf",
    ] {
        let ex = extracted(name);
        assert_eq!(ex.provenance.adapter, "pdf", "{name}");
        assert_eq!(
            ex.provenance.text_layer_quality,
            TextLayerQuality::Usable,
            "{name} is a clean born-digital layer",
        );
        assert!(!ex.blocks.is_empty(), "{name} has body blocks");
        assert!(
            ex.blocks.iter().all(|b| b.kind == BlockKind::Body),
            "{name}: a PDF has no semantic block kinds — every block is Body",
        );
        assert!(
            ex.provenance.skipped_units.is_empty(),
            "{name}: a clean fixture skips no pages",
        );
    }
}

#[test]
fn prose_en_blocks_carry_english_prose_in_page_order() {
    if !pdfium_available() {
        return;
    }
    let ex = extracted("prose_en.pdf");

    // The fixture is three pages. The line-heuristic joins each page's
    // wrapped lines into a small number of blocks rather than splitting
    // true paragraphs — "page lumps" — so the block count stays far
    // below the source's paragraph count. This pins that known
    // limitation; coordinate reconstruction will raise the count.
    assert!(
        (3..=8).contains(&ex.blocks.len()),
        "expected page-lump blocks, got {}",
        ex.blocks.len(),
    );

    // source_unit is the 0-based page index, and blocks stay in page
    // order.
    let units: Vec<u32> = ex.blocks.iter().map(|b| b.source_unit).collect();
    assert!(
        units.windows(2).all(|w| w[0] <= w[1]),
        "page order: {units:?}"
    );
    assert!(units.iter().all(|&u| u < 3), "three pages: {units:?}");

    assert!(
        joined_text(&ex).contains("A margin is not wasted paper"),
        "a known sentence survives extraction",
    );
}

#[test]
fn prose_cjk_blocks_carry_joined_ideographic_text() {
    if !pdfium_available() {
        return;
    }
    let ex = extracted("prose_cjk.pdf");

    assert!(
        (2..=6).contains(&ex.blocks.len()),
        "expected page-lump blocks, got {}",
        ex.blocks.len(),
    );

    // A distinctive phrase — the fixture's first chapter heading —
    // must survive extraction. Spelled with \u escapes so this file
    // stays ASCII; CJK literals belong only in fixture files.
    assert!(
        joined_text(&ex).contains("\u{7eb8}\u{7684}\u{6765}\u{5386}"),
        "a known CJK phrase survives extraction",
    );
}

#[test]
fn encrypted_restricted_pdf_opens_without_a_password() {
    if !pdfium_available() {
        return;
    }
    // Owner-password-only encryption: the file opens with no password.
    // The fixture is prose_en re-saved with encryption, so its
    // extracted content must be identical.
    assert_eq!(
        extracted("encrypted_restricted.pdf"),
        extracted("prose_en.pdf"),
        "an owner-password file extracts exactly like its plaintext",
    );
}

#[test]
fn encrypted_userpw_pdf_is_drm_protected() {
    if !pdfium_available() {
        return;
    }
    // A user (open) password is set: the file cannot be opened without
    // it. This project does not decrypt — it rejects the file.
    let err = extract(&pdf_fixture("encrypted_userpw.pdf")).expect_err("user password");
    assert!(matches!(err, ExtractError::DrmProtected), "got {err:?}");
}

#[test]
fn image_only_pdf_routes_to_ocr() {
    if !pdfium_available() {
        return;
    }
    // A pure-raster page with no text layer: nothing to extract, so the
    // adapter routes it to OCR rather than producing an empty success.
    match extract(&pdf_fixture("image_only.pdf")) {
        Ok(ExtractOutcome::NeedsOcr { reason }) => {
            assert!(!reason.is_empty(), "the OCR-routing reason is recorded");
        }
        other => panic!("expected NeedsOcr, got {other:?}"),
    }
}

#[test]
fn corrupt_pdf_is_reported_as_a_corrupt_file() {
    if !pdfium_available() {
        return;
    }
    // A file with the %PDF header but no recoverable objects: a
    // structural, whole-file failure, distinct from a missing text
    // layer.
    let err = extract(&pdf_fixture("corrupt.pdf")).expect_err("corrupt file");
    assert!(
        matches!(err, ExtractError::CorruptFile { .. }),
        "got {err:?}"
    );
}

#[test]
fn toc_deep_preserves_outline_depth_and_anchors_every_entry() {
    if !pdfium_available() {
        return;
    }
    let ex = extracted("toc_deep.pdf");

    // The fixture's /Outline carries 18 headings, four levels deep.
    assert_eq!(ex.toc.entries.len(), 18, "every outline entry is lifted");

    let depths: BTreeSet<u8> = ex.toc.entries.iter().map(|e| e.depth).collect();
    assert_eq!(
        depths,
        BTreeSet::from([0, 1, 2, 3]),
        "all four outline levels survive flattening",
    );

    // Every heading has a target page, so every entry anchors.
    assert!(
        ex.toc.entries.iter().all(|e| e.start_block.is_some()),
        "every outline entry anchors to a block",
    );

    // The handbook runs several pages, so anchoring spreads across
    // blocks rather than collapsing every entry onto block 0.
    let anchors: BTreeSet<usize> = ex
        .toc
        .entries
        .iter()
        .filter_map(|e| e.start_block)
        .collect();
    assert!(
        anchors.len() >= 3,
        "anchors spread across pages: {anchors:?}"
    );
}

#[test]
fn prose_en_transcribes_clean_info_metadata() {
    if !pdfium_available() {
        return;
    }
    let ex = extracted("prose_en.pdf");

    assert_eq!(ex.biblio.title.as_deref(), Some("The Printed Page"));
    // The /Info CreationDate is pinned in the fixture source.
    assert_eq!(ex.biblio.year, Some(2011));
    assert_eq!(ex.biblio.contributors.len(), 1, "one /Info author");
    assert_eq!(ex.biblio.contributors[0].role, ContributorRole::Author);
}

#[test]
fn biblio_garbage_transcribes_unreliable_info_verbatim() {
    if !pdfium_available() {
        return;
    }
    let ex = extracted("biblio_garbage.pdf");

    // /Info is deliberately unreliable: a Word working-file name in the
    // title slot and a production date unrelated to the publication
    // year. extract transcribes it as-is — reconciling it against the
    // page text is the METADATA stage's job, not extract's.
    assert_eq!(
        ex.biblio.title.as_deref(),
        Some("Microsoft Word - chapter_revised_FINAL (2).docx"),
    );
    assert_eq!(
        ex.biblio.year,
        Some(2023),
        "the production date is transcribed, not the real publication year",
    );
}

#[test]
fn pdf_extraction_is_deterministic() {
    if !pdfium_available() {
        return;
    }
    // The determinism invariant: same file => identical outcome. Holds
    // for the OCR-routing outcome too, not just extracted text.
    for name in [
        "prose_en.pdf",
        "prose_cjk.pdf",
        "two_column.pdf",
        "toc_deep.pdf",
        "biblio_garbage.pdf",
        "encrypted_restricted.pdf",
        "image_only.pdf",
    ] {
        let path = pdf_fixture(name);
        let first = extract(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        let second = extract(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(first, second, "{name} extracts deterministically");
    }
}
