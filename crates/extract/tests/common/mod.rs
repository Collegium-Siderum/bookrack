// SPDX-License-Identifier: Apache-2.0

//! Shared support for the `extract` integration tests.
//!
//! EPUB fixtures are checked in unzipped — one directory per fixture
//! under `tests/fixtures/epub/` — so every byte is reviewable as plain
//! text in the repo. rbook needs a real archive, so each test packs its
//! fixture into a throwaway `.epub` on the fly.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// A fixture packed into a temporary `.epub`. Holds the temp directory
/// open for the lifetime of the value, so `path` stays valid.
pub struct PackedEpub {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
}

/// Pack the unzipped EPUB fixture `tests/fixtures/epub/<name>` into a
/// throwaway `.epub`.
///
/// The `mimetype` entry is written first and stored uncompressed, as
/// the EPUB OCF container spec requires; the fixture directories omit
/// it and the canonical value is supplied here.
pub fn pack_epub(name: &str) -> PackedEpub {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/epub")
        .join(name);
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("book.epub");

    let mut zip = ZipWriter::new(File::create(&path).expect("create epub file"));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", stored).expect("mimetype entry");
    zip.write_all(b"application/epub+zip")
        .expect("write mimetype");

    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut files = Vec::new();
    collect_files(&src, &src, &mut files);
    // A stable order keeps the packed archive itself deterministic.
    files.sort();
    for rel in files {
        let mut buf = Vec::new();
        File::open(src.join(&rel))
            .expect("open fixture file")
            .read_to_end(&mut buf)
            .expect("read fixture file");
        zip.start_file(rel.to_string_lossy().replace('\\', "/"), deflated)
            .expect("archive entry");
        zip.write_all(&buf).expect("write archive entry");
    }
    zip.finish().expect("finish archive");

    PackedEpub { _dir: dir, path }
}

/// Collect file paths under `dir`, relative to `root`.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read fixture dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else {
            out.push(
                path.strip_prefix(root)
                    .expect("path under root")
                    .to_path_buf(),
            );
        }
    }
}
