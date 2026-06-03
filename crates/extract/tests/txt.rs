// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the plain-text adapter, driven by a fixture
//! under `tests/fixtures/txt/`.

mod common;

use std::path::{Path, PathBuf};

use bookrack_extract::{BlockKind, extract};
use common::extracted;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/txt")
        .join(name)
}

#[test]
fn utf8_text_yields_blocks_and_a_chapter_toc() {
    let ex = extracted(&fixture("web_novel.txt"));

    // Volume markers nest at depth 0, chapter markers at depth 1.
    let depths: Vec<u8> = ex.toc.entries.iter().map(|e| e.depth).collect();
    assert_eq!(depths, vec![0, 1, 1, 0, 1]);
    assert!(ex.toc.entries.iter().all(|e| e.start_block.is_some()));
    assert!(ex.blocks.iter().any(|b| b.kind == BlockKind::Body));
    assert!(ex.provenance.extractor_version.contains("encoding=utf-8"));
}

#[test]
fn legacy_gbk_text_is_decoded_via_gb18030() {
    // The UTF-8 fixture re-encoded to GB18030 must extract to exactly
    // the same blocks and TOC — only the stamped encoding differs.
    let utf8 = std::fs::read(fixture("web_novel.txt")).expect("read fixture");
    let text = String::from_utf8(utf8).expect("fixture is utf-8");
    let (gbk, _, _) = encoding_rs::GB18030.encode(&text);
    let gbk = gbk.into_owned();

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("legacy.txt");
    std::fs::write(&path, &gbk).expect("write gbk file");

    let from_gbk = extracted(&path);
    let from_utf8 = extracted(&fixture("web_novel.txt"));

    assert_eq!(from_gbk.blocks, from_utf8.blocks);
    assert_eq!(from_gbk.toc, from_utf8.toc);
    assert!(
        from_gbk
            .provenance
            .extractor_version
            .contains("encoding=gb18030"),
    );
}

#[test]
fn txt_extraction_is_deterministic() {
    let path = fixture("web_novel.txt");
    let first = extract(&path, &common::default_extract_toggles()).expect("first extract");
    let second = extract(&path, &common::default_extract_toggles()).expect("second extract");
    assert_eq!(first, second);
}

#[test]
fn disabling_txt_toc_collapses_headings_to_body() {
    use bookrack_audit_profile::ExtractToggles;
    use bookrack_extract::ExtractOutcome;

    let toggles = ExtractToggles {
        txt_toc_enabled: false,
        ..ExtractToggles::default()
    };
    let outcome = extract(&fixture("web_novel.txt"), &toggles).expect("extract");
    let ExtractOutcome::Extracted(ex) = outcome else {
        panic!("expected an extracted text layer");
    };
    assert!(ex.blocks.iter().all(|b| b.kind == BlockKind::Body));
    assert!(ex.toc.entries.is_empty());
}
