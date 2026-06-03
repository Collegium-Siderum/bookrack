// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the standalone-HTML adapter, driven by a
//! fixture under `tests/fixtures/html/`.

mod common;

use std::path::{Path, PathBuf};

use bookrack_extract::{BlockKind, ContributorRole, extract};
use common::extracted;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/html")
        .join(name)
}

#[test]
fn standalone_infers_toc_from_heading_hierarchy() {
    let ex = extracted(&fixture("standalone.html"));

    // No nav document: the TOC is inferred from <h1>-<h6>, depth taken
    // straight from the heading level.
    let depths: Vec<u8> = ex.toc.entries.iter().map(|e| e.depth).collect();
    assert_eq!(depths, vec![0, 1, 2, 1]);

    let labels: Vec<&str> = ex.toc.entries.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(
        labels,
        ["Part One", "A Section", "A Subsection", "Another Section"],
    );

    // Each entry anchors to its own heading block.
    for entry in &ex.toc.entries {
        let idx = entry.start_block.expect("entry anchored");
        assert!(matches!(ex.blocks[idx].kind, BlockKind::Heading { .. }));
    }
}

#[test]
fn standalone_reads_head_metadata() {
    let ex = extracted(&fixture("standalone.html"));
    let b = &ex.biblio;

    assert_eq!(b.title.as_deref(), Some("A Standalone Synthetic Document"));
    assert_eq!(b.language.as_deref(), Some("en"));
    assert_eq!(b.contributors.len(), 1);
    assert_eq!(b.contributors[0].name, "Henry Htmlwright");
    assert_eq!(b.contributors[0].role, ContributorRole::Author);
}

#[test]
fn html_extraction_is_deterministic() {
    let path = fixture("standalone.html");
    let first = extract(&path, &common::default_extract_toggles()).expect("first extract");
    let second = extract(&path, &common::default_extract_toggles()).expect("second extract");
    assert_eq!(first, second);
}
