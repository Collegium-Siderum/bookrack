// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the EPUB adapter, driven by unzipped fixtures
//! under `tests/fixtures/epub/` (see `tests/common/mod.rs`).

mod common;

use bookrack_extract::{BlockKind, ContributorRole, ExtractError, extract};
use common::{extracted, pack_epub};

#[test]
fn omnibus_keeps_toc_hierarchy_and_anchors_every_entry() {
    let epub = pack_epub("omnibus");
    let ex = extracted(&epub.path);

    // The nav nests part > work > chapter; the depth tags must survive,
    // since a flattened TOC would lose the middle level.
    let depths: Vec<u8> = ex.toc.entries.iter().map(|e| e.depth).collect();
    assert!(depths.contains(&0), "a topmost entry, got {depths:?}");
    assert!(depths.contains(&1), "a mid-level entry, got {depths:?}");
    assert!(depths.contains(&2), "a leaf entry, got {depths:?}");

    // Every entry resolves to a block — including one whose own
    // document is an empty part-divider page, which must resolve
    // forward to where the prose actually begins.
    assert!(
        ex.toc.entries.iter().all(|e| e.start_block.is_some()),
        "every TOC entry anchored",
    );
}

#[test]
fn omnibus_classifies_footnote_and_caption_blocks() {
    let epub = pack_epub("omnibus");
    let ex = extracted(&epub.path);

    let kinds: Vec<BlockKind> = ex.blocks.iter().map(|b| b.kind).collect();
    assert!(kinds.contains(&BlockKind::Footnote), "a footnote block");
    assert!(kinds.contains(&BlockKind::Caption), "a caption block");
    assert!(
        kinds.iter().any(|k| matches!(k, BlockKind::Heading { .. })),
        "a heading block",
    );
    assert!(kinds.contains(&BlockKind::Body), "a body block");
}

#[test]
fn extraction_is_deterministic() {
    let epub = pack_epub("omnibus");
    let first = extract(&epub.path, &common::default_extract_toggles()).expect("first extract");
    let second = extract(&epub.path, &common::default_extract_toggles()).expect("second extract");
    // The determinism invariant: same file => byte-identical Extraction.
    assert_eq!(first, second);
}

#[test]
fn flat_transcribes_bibliographic_metadata() {
    let epub = pack_epub("flat");
    let ex = extracted(&epub.path);
    let b = &ex.biblio;

    assert_eq!(b.title.as_deref(), Some("A Flat Single Work"));
    assert_eq!(b.publisher.as_deref(), Some("Synthetic Press"));
    assert_eq!(b.year, Some(2011));
    assert_eq!(b.language.as_deref(), Some("en"));
    assert_eq!(b.isbn.as_deref(), Some("9780000000002"));

    let named = |role| {
        b.contributors
            .iter()
            .find(|c| c.role == role)
            .map(|c| c.name.as_str())
    };
    assert_eq!(named(ContributorRole::Author), Some("Ada Authorman"));
    assert_eq!(named(ContributorRole::Translator), Some("Tomas Translator"));
}

#[test]
fn headings_with_no_prose_is_empty_extraction() {
    let epub = pack_epub("headings_only");
    let err = extract(&epub.path, &common::default_extract_toggles()).expect_err("no body blocks");
    assert!(matches!(err, ExtractError::EmptyExtraction), "got {err:?}");
}

#[test]
fn a_non_archive_is_a_corrupt_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("broken.epub");
    std::fs::write(&path, b"this is plainly not a zip archive").expect("write");

    let err = extract(&path, &common::default_extract_toggles()).expect_err("not an archive");
    assert!(
        matches!(err, ExtractError::CorruptFile { .. }),
        "got {err:?}"
    );
}

#[test]
fn a_zip_missing_the_ocf_container_xml_is_a_corrupt_file() {
    use std::fs::File;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    // A valid ZIP that even carries the OCF mimetype entry, yet omits
    // `META-INF/container.xml`. rbook can open the archive but cannot
    // locate the package document, which surfaces from `Epub::open` and
    // is mapped to `ExtractError::CorruptFile`.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("no_container.epub");
    {
        let mut zip = ZipWriter::new(File::create(&path).expect("create epub"));
        zip.start_file(
            "mimetype",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .expect("mimetype entry");
        zip.write_all(b"application/epub+zip")
            .expect("write mimetype");
        zip.finish().expect("finish archive");
    }

    let err = extract(&path, &common::default_extract_toggles())
        .expect_err("missing container.xml must fail");
    assert!(
        matches!(err, ExtractError::CorruptFile { .. }),
        "got {err:?}"
    );
}
