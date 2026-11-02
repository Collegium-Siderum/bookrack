// SPDX-License-Identifier: Apache-2.0

//! Two properties a comparison against a stored envelope rests on: one
//! input extracted twice yields equal extractions, and an extraction
//! read back from its envelope equals the one written.
//!
//! The PDF fixtures need the PDFium native library and skip without it
//! (see [`common::pdfium_available`]); CI always provides the binary.
//! They are the ones whose blocks carry [`bookrack_extract::BlockStyle`]
//! and its `f32` fields.

mod common;

use std::path::{Path, PathBuf};

use bookrack_core::ItemKind;
use bookrack_extract::{Extraction, envelope_filename, read_envelope, write_envelope};
use common::{extracted, pack_epub, pdf_fixture, pdfium_available};

const SOURCE_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn fixture(dir: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name)
}

/// Fail with the first part of `left` that differs from `right`, so a
/// mismatch names a block index rather than dumping two extractions.
fn assert_same(left: &Extraction, right: &Extraction, what: &str) {
    assert_eq!(
        left.blocks.len(),
        right.blocks.len(),
        "{what}: block counts differ"
    );
    if let Some(i) = (0..left.blocks.len()).find(|&i| left.blocks[i] != right.blocks[i]) {
        panic!(
            "{what}: block {i} differs:\n  {:?}\n  {:?}",
            left.blocks[i], right.blocks[i]
        );
    }
    assert_eq!(left.toc, right.toc, "{what}: tocs differ");
    assert_eq!(left.biblio, right.biblio, "{what}: biblios differ");
    assert_eq!(
        left.provenance, right.provenance,
        "{what}: provenances differ"
    );
    assert!(left == right, "{what}: extractions differ");
}

/// Extract `path` twice and compare the two, then write the first to an
/// envelope, read it back, and compare again.
fn assert_alike_and_round_trips(label: &str, path: &Path) {
    let first = extracted(path);
    let second = extracted(path);
    assert_same(&first, &second, &format!("{label}: two extractions"));

    let dir = tempfile::tempdir().expect("tempdir");
    let envelope = dir.path().join(envelope_filename(ItemKind::Book, 1));
    write_envelope(&envelope, &first, 1, SOURCE_SHA256).expect("write envelope");
    let read = read_envelope(&envelope).expect("read envelope");
    assert_same(
        &first,
        &read.extraction,
        &format!("{label}: extraction against its envelope"),
    );
}

#[test]
fn born_digital_fixtures_extract_alike_and_survive_their_envelope() {
    for name in ["web_novel.txt", "multilingual_headings.txt"] {
        assert_alike_and_round_trips(name, &fixture("txt", name));
    }
    assert_alike_and_round_trips("standalone.html", &fixture("html", "standalone.html"));
    for name in ["flat", "omnibus"] {
        let epub = pack_epub(name);
        assert_alike_and_round_trips(name, &epub.path);
    }
}

#[test]
fn pdf_fixtures_extract_alike_and_survive_their_envelope() {
    if !pdfium_available() {
        return;
    }
    for name in [
        "prose_en.pdf",
        "prose_cjk.pdf",
        "toc_deep.pdf",
        "two_column.pdf",
        "biblio_garbage.pdf",
        "paper_cn.pdf",
    ] {
        assert_alike_and_round_trips(name, &pdf_fixture(name));
    }
}
