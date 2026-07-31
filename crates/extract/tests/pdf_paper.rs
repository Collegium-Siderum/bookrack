// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the paper-mode extractors —
//! [`extract_paper_abstract`] and [`extract_paper_metadata_text`] —
//! driven by real PDF fixtures under `tests/fixtures/pdf/`.
//!
//! The inline unit tests in `pdf_paper.rs` cover the anchor and
//! References regexes and the scan-window core over synthetic page
//! text; these tests pin the same promises through PDFium against the
//! compiled fixtures, one per documented reason tag.

mod common;

use bookrack_extract::{extract_paper_abstract, extract_paper_metadata_text};
use common::{pdf_fixture, pdfium_available};

/// The abstract and its reason tag for a fixture, unwrapped.
fn abstract_of(name: &str) -> (String, &'static str) {
    extract_paper_abstract(&pdf_fixture(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .unwrap_or_else(|| panic!("{name}: expected an abstract"))
}

/// The metadata-scan text for a fixture, unwrapped.
fn metadata_of(name: &str) -> String {
    extract_paper_metadata_text(&pdf_fixture(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .unwrap_or_else(|| panic!("{name}: expected metadata text"))
}

#[test]
fn the_english_abstract_heading_anchors_the_abstract_band() {
    if !pdfium_available() {
        return;
    }
    let (text, reason) = abstract_of("two_column.pdf");
    assert_eq!(reason, "heading-en");
    assert!(
        text.contains("The length of a coastline is not a fixed quantity"),
        "the band body follows the anchor, got {text:?}",
    );
    assert!(
        text.contains("scale of measurement is reported beside it"),
        "the band runs to its closing sentence, got {text:?}",
    );
    assert!(
        !text.contains("Ask how long a coastline is"),
        "the Introduction stop-heading closes the band, got {text:?}",
    );
}

#[test]
fn prose_without_an_anchor_falls_back_to_the_first_pages() {
    if !pdfium_available() {
        return;
    }
    let (text, reason) = abstract_of("prose_en.pdf");
    assert_eq!(reason, "fallback-first-pages");
    assert!(
        text.contains("A margin is not wasted paper"),
        "the fallback carries page-one prose, got {text:?}",
    );
}

#[test]
fn the_cjk_abstract_heading_anchors_the_abstract_band() {
    if !pdfium_available() {
        return;
    }
    // "zhai yao" heading; the band body opens on the coastline-length
    // sentence and the keywords line ("guan jian ci") closes it.
    let (text, reason) = abstract_of("paper_cn.pdf");
    assert_eq!(reason, "heading-cn");
    assert!(
        text.contains("\u{6D77}\u{5CB8}\u{7EBF}\u{7684}\u{957F}\u{5EA6}"),
        "the band body follows the anchor, got {text:?}",
    );
    assert!(
        !text.contains("\u{53EF}\u{6838}\u{5BF9}\u{6027}"),
        "the keywords stop-heading closes the band, got {text:?}",
    );
}

#[test]
fn the_metadata_window_ends_before_the_bibliography() {
    if !pdfium_available() {
        return;
    }
    let text = metadata_of("paper_cn.pdf");
    assert!(
        text.contains("Journal of Synthetic Coastlines"),
        "front-matter lines stay inside the window, got {text:?}",
    );
    assert!(
        !text.contains("\u{53C2}\u{8003}\u{6587}\u{732E}"),
        "the References heading itself is cut, got {text:?}",
    );
    assert!(
        !text.contains("Bibliographia Prima"),
        "bibliography entries sit behind the window, got {text:?}",
    );
}

#[test]
fn the_fullwidth_doi_banner_folds_to_ascii() {
    if !pdfium_available() {
        return;
    }
    let text = metadata_of("paper_cn.pdf");
    assert!(
        text.chars()
            .all(|c| !('\u{FF01}'..='\u{FF5E}').contains(&c)),
        "no fullwidth form survives the fold",
    );
    assert!(
        text.contains("10.1234/bkr.2024.00753"),
        "the folded DOI is matchable as ASCII, got {text:?}",
    );
}
