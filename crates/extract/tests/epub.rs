// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the EPUB adapter, driven by unzipped fixtures
//! under `tests/fixtures/epub/` (see `tests/common/mod.rs`).

mod common;

use bookrack_extract::{BlockKind, ContributorRole, ExtractError, extract};
use common::{extracted, pack_epub, pack_epub_dir};

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
    let first = extract(
        &epub.path,
        &common::default_audit_profile(),
        &common::default_heading_patterns(),
    )
    .expect("first extract");
    let second = extract(
        &epub.path,
        &common::default_audit_profile(),
        &common::default_heading_patterns(),
    )
    .expect("second extract");
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
    let err = extract(
        &epub.path,
        &common::default_audit_profile(),
        &common::default_heading_patterns(),
    )
    .expect_err("no body blocks");
    assert!(matches!(err, ExtractError::EmptyExtraction), "got {err:?}");
}

#[test]
fn a_non_archive_is_a_corrupt_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("broken.epub");
    std::fs::write(&path, b"this is plainly not a zip archive").expect("write");

    let err = extract(
        &path,
        &common::default_audit_profile(),
        &common::default_heading_patterns(),
    )
    .expect_err("not an archive");
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

    let err = extract(
        &path,
        &common::default_audit_profile(),
        &common::default_heading_patterns(),
    )
    .expect_err("missing container.xml must fail");
    assert!(
        matches!(err, ExtractError::CorruptFile { .. }),
        "got {err:?}"
    );
}

#[test]
fn an_opf_comment_with_a_non_utf8_byte_is_a_corrupt_file() {
    let tree = common::copy_epub_fixture("flat");
    // The unmodified copy packs and extracts through the same path, so
    // the failure below comes from the one byte changed.
    extracted(&pack_epub_dir(tree.path()).path);

    let opf = tree.path().join("OEBPS/content.opf");
    let original = std::fs::read(&opf).expect("read OPF");
    let start_tag: &[u8] = b"<manifest>";
    let at: Vec<usize> = original
        .windows(start_tag.len())
        .enumerate()
        .filter(|(_, w)| *w == start_tag)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(at.len(), 1, "one <manifest> start tag in the fixture OPF");
    let mut patched = original[..at[0]].to_vec();
    // 0xE9 is Latin-1 for a small e with acute; on its own it is not
    // valid UTF-8.
    patched.extend_from_slice(b"<!-- caf\xe9 -->");
    patched.extend_from_slice(&original[at[0]..]);
    std::fs::write(&opf, patched).expect("write OPF");

    let err = extract(
        &pack_epub_dir(tree.path()).path,
        &common::default_audit_profile(),
        &common::default_heading_patterns(),
    )
    .expect_err("an OPF holding a non-UTF-8 byte must not open");
    match err {
        ExtractError::CorruptFile { detail } => assert!(
            detail.contains("cannot decode input using UTF-8"),
            "the detail must name the UTF-8 decode failure, got {detail}"
        ),
        other => panic!("expected CorruptFile, got {other:?}"),
    }
}

#[test]
fn a_spine_document_larger_than_its_declared_size_is_a_malformed_package() {
    let tree = common::copy_epub_fixture("flat");
    let member = "OEBPS/c1.xhtml";
    let actual = u32::try_from(
        std::fs::metadata(tree.path().join(member))
            .expect("spine document")
            .len(),
    )
    .expect("fixture fits a 32-bit size field");
    let packed = pack_epub_dir(tree.path());
    // The archive as packed extracts, so the failure below comes from
    // the size fields alone.
    extracted(&packed.path);

    let mut bytes = std::fs::read(&packed.path).expect("read archive");
    shrink_declared_size(&mut bytes, member, actual, 20);
    std::fs::write(&packed.path, &bytes).expect("write archive");

    let err = extract(
        &packed.path,
        &common::default_audit_profile(),
        &common::default_heading_patterns(),
    )
    .expect_err("a spine document larger than its declared size must not be read");
    match err {
        ExtractError::MalformedPackage { detail } => assert!(
            detail.contains("larger than its declared uncompressed size"),
            "the detail must name the size mismatch, got {detail}"
        ),
        other => panic!("expected MalformedPackage, got {other:?}"),
    }
}

/// Rewrite the declared uncompressed size of `member` in the ZIP
/// archive `bytes` from `actual` to `actual - by`, in both its central
/// directory record and its local file header.
///
/// Panics unless the central directory holds exactly one record named
/// `member`, that record points at a local header carrying the same
/// name, the sizes are recorded in the headers rather than in a
/// trailing data descriptor, and both headers declare `actual`.
fn shrink_declared_size(bytes: &mut [u8], member: &str, actual: u32, by: u32) {
    fn u16_at(b: &[u8], at: usize) -> usize {
        usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes"))
    }

    let eocd = bytes
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .expect("end of central directory record");
    let records = u16_at(bytes, eocd + 10);
    let mut at = u32_at(bytes, eocd + 16) as usize;
    let mut named = Vec::new();
    for _ in 0..records {
        assert_eq!(
            &bytes[at..at + 4],
            b"PK\x01\x02",
            "central directory record at {at}"
        );
        let name_len = u16_at(bytes, at + 28);
        let extra_len = u16_at(bytes, at + 30);
        let comment_len = u16_at(bytes, at + 32);
        if &bytes[at + 46..at + 46 + name_len] == member.as_bytes() {
            named.push(at);
        }
        at += 46 + name_len + extra_len + comment_len;
    }
    assert_eq!(named.len(), 1, "{member} has one central directory record");
    let central = named[0];

    let local = u32_at(bytes, central + 42) as usize;
    assert_eq!(
        &bytes[local..local + 4],
        b"PK\x03\x04",
        "local file header of {member}"
    );
    let local_name_len = u16_at(bytes, local + 26);
    assert_eq!(
        &bytes[local + 30..local + 30 + local_name_len],
        member.as_bytes(),
        "the central record points at {member}'s own local header",
    );
    assert_eq!(
        u16_at(bytes, local + 6) & 0x0008,
        0,
        "{member} records its sizes in the headers, not a data descriptor",
    );

    for size_at in [central + 24, local + 22] {
        assert_eq!(
            u32_at(bytes, size_at),
            actual,
            "declared uncompressed size of {member} at offset {size_at}",
        );
        bytes[size_at..size_at + 4].copy_from_slice(&(actual - by).to_le_bytes());
    }
}
