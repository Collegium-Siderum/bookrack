// SPDX-License-Identifier: Apache-2.0

//! Byte-stable `tar.gz` writer.
//!
//! Given a staging directory and an output path, walk the directory in
//! sorted order and append every regular file to a gzip-wrapped tar
//! stream. Every header field that would otherwise leak host state —
//! mtime, uid, gid, uname, gname — is set to a fixed value so two runs
//! over the same inputs produce identical bytes on disk.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::DiagnoseError;

/// Compression level for the gzip wrapper. Matches the `gzip(1)`
/// default, which trades modestly slower writes for noticeably smaller
/// bundles.
const GZIP_LEVEL: u32 = 6;

/// Append every regular file under `staging_dir` to a `tar.gz` written
/// at `out_path`. The output is byte-stable for a fixed input tree.
pub fn write_bundle(staging_dir: &Path, out_path: &Path) -> Result<(), DiagnoseError> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut entries = Vec::new();
    walk(staging_dir, staging_dir, &mut entries)?;
    entries.sort_by(|a, b| a.relative.cmp(&b.relative));

    let file = File::create(out_path)?;
    let gz = GzEncoder::new(BufWriter::new(file), Compression::new(GZIP_LEVEL));
    let mut builder = tar::Builder::new(gz);
    builder.mode(tar::HeaderMode::Deterministic);
    for entry in &entries {
        let src = File::open(&entry.absolute)?;
        let len = std::fs::metadata(&entry.absolute)?.len();
        let mut header = tar::Header::new_gnu();
        header.set_size(len);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, &entry.relative, BufReader::new(src))
            .map_err(DiagnoseError::from)?;
    }
    // `into_inner` writes the tar end-marker and returns the wrapped
    // gz encoder; `finish` then completes the gzip stream and returns
    // the underlying BufWriter, which flushes on drop.
    let gz = builder.into_inner()?;
    let _buf = gz.finish()?;
    Ok(())
}

struct Entry {
    absolute: std::path::PathBuf,
    relative: String,
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> Result<(), DiagnoseError> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            out.push(Entry {
                absolute: path,
                relative: rel,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Build a small staging tree and verify that two writes produce
    /// byte-identical archives.
    #[test]
    fn write_bundle_is_byte_stable_for_fixed_input() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("stage");
        std::fs::create_dir_all(staging.join("logs")).unwrap();
        std::fs::write(staging.join("env.txt"), b"a=1\n").unwrap();
        std::fs::write(staging.join("logs/y"), b"yyyyy").unwrap();
        std::fs::write(staging.join("logs/x"), b"xxxx").unwrap();

        let first = tmp.path().join("first.tar.gz");
        let second = tmp.path().join("second.tar.gz");
        write_bundle(&staging, &first).unwrap();
        write_bundle(&staging, &second).unwrap();
        let a = std::fs::read(&first).unwrap();
        let b = std::fs::read(&second).unwrap();
        assert_eq!(a, b, "two runs over the same input must match");
    }

    /// Read the archive back and verify every entry's header fields
    /// are zeroed-out, so the bundle does not carry uid / mtime
    /// information from the host.
    #[test]
    fn write_bundle_writes_zeroed_headers() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("stage");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("env.txt"), b"v=2\n").unwrap();
        let out = tmp.path().join("bundle.tar.gz");
        write_bundle(&staging, &out).unwrap();

        let raw = std::fs::read(&out).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
        let mut tar_bytes = Vec::new();
        decoder.read_to_end(&mut tar_bytes).unwrap();
        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let mut found = 0;
        for entry in archive.entries().unwrap() {
            let e = entry.unwrap();
            assert_eq!(e.header().mtime().unwrap(), 0);
            assert_eq!(e.header().uid().unwrap(), 0);
            assert_eq!(e.header().gid().unwrap(), 0);
            found += 1;
        }
        assert_eq!(found, 1, "one entry expected, got {found}");
    }
}
