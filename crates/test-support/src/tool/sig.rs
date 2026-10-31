// SPDX-License-Identifier: Apache-2.0

//! Tree signatures, their differences, and the byte escaping that keeps
//! a signature line and a TSV row splittable whatever a name contains.

use std::io::{self, Read};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::{Expect, ToolPaths, write_atomic};

/// Encode `bytes` so the result has no space, tab, newline or carriage
/// return: `\` becomes `\\`, those four become `\t` `\n` `\r` `\x20`,
/// every other byte outside `0x21..=0x7E` becomes `\xNN`.
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str(r"\\"),
            b'\t' => out.push_str(r"\t"),
            b'\n' => out.push_str(r"\n"),
            b'\r' => out.push_str(r"\r"),
            0x21..=0x7E => out.push(b as char),
            _ => out.push_str(&format!(r"\x{b:02x}")),
        }
    }
    out
}

/// Decode what [`escape`] produced.
pub fn unescape(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        let Some(&next) = bytes.get(i + 1) else {
            return Err("a trailing backslash".to_string());
        };
        match next {
            b'\\' => out.push(b'\\'),
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'x' => {
                let hex = bytes
                    .get(i + 2..i + 4)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .ok_or_else(|| format!("a short \\x escape at {i}"))?;
                let value = u8::from_str_radix(hex, 16)
                    .map_err(|_| format!("a bad \\x escape {hex:?} at {i}"))?;
                out.push(value);
                i += 4;
                continue;
            }
            other => return Err(format!("an unknown escape \\{} at {i}", other as char)),
        }
        i += 2;
    }
    Ok(out)
}

/// The SHA-256 of `bytes`, lowercase hex.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The SHA-256 of the file at `path`, lowercase hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// One line per entry under `root`, sorted by bytes:
/// `<kind> <size|-> <sha256|-> <escaped relpath>`. Only a regular file
/// records a size and a digest; a socket, FIFO or device is not opened.
/// A symbolic link is an error. An entry that vanishes between being
/// listed and being read is recorded as `vanished - -`.
pub fn signature(root: &Path) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound && path != root => {
                lines.push(format!("vanished - - {}", escape(relpath(root, &path))));
                continue;
            }
            Err(err) => return Err(err),
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Err(io::Error::other(format!(
                "{} is a symbolic link; a tree to sign must not hold one",
                path.display()
            )));
        }
        let rel = escape(relpath(root, &path));
        if kind.is_dir() {
            if path != root {
                lines.push(format!("dir - - {rel}"));
            }
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else if kind.is_file() {
            let sha = match sha256_file(&path) {
                Ok(sha) => sha,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    lines.push(format!("vanished - - {rel}"));
                    continue;
                }
                Err(err) => return Err(err),
            };
            lines.push(format!("file {} {sha} {rel}", meta.len()));
        } else if kind.is_socket() {
            lines.push(format!("socket - - {rel}"));
        } else if kind.is_fifo() {
            lines.push(format!("fifo - - {rel}"));
        } else {
            lines.push(format!("device - - {rel}"));
        }
    }
    lines.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(lines)
}

fn relpath<'a>(root: &Path, path: &'a Path) -> &'a [u8] {
    use std::os::unix::ffi::OsStrExt;
    path.strip_prefix(root)
        .map(|p| p.as_os_str().as_bytes())
        .unwrap_or(b"")
}

/// The difference between two signatures: `+ <line>` for an entry only
/// in `after`, `- <line>` for one only in `before`, `~ <relpath>` for
/// one whose line changed.
pub fn diff(before: &[String], after: &[String]) -> Vec<String> {
    use std::collections::BTreeMap;
    let key = |line: &str| line.rsplit(' ').next().unwrap_or("").to_string();
    let b: BTreeMap<String, &String> = before.iter().map(|l| (key(l), l)).collect();
    let a: BTreeMap<String, &String> = after.iter().map(|l| (key(l), l)).collect();
    let mut out = Vec::new();
    for (k, line) in &b {
        match a.get(k) {
            None => out.push(format!("- {line}")),
            Some(other) if other != line => out.push(format!("~ {k}")),
            Some(_) => {}
        }
    }
    for (k, line) in &a {
        if !b.contains_key(k) {
            out.push(format!("+ {line}"));
        }
    }
    out
}

/// A diff compressed to its top-level components, for a summary cell:
/// `+roots -cwd ~registry.toml`, or `=` when nothing changed.
pub fn compact_diff(diff: &[String]) -> String {
    use std::collections::BTreeSet;
    let mut parts = BTreeSet::new();
    for line in diff {
        let (mark, rest) = line.split_at(1);
        let rel = rest.trim_start().rsplit(' ').next().unwrap_or("");
        let top = rel.split('/').next().unwrap_or(rel);
        parts.insert(format!("{mark}{top}"));
    }
    if parts.is_empty() {
        "=".to_string()
    } else {
        parts.into_iter().collect::<Vec<_>>().join(" ")
    }
}

/// Append `line` to the tool-owned file at `path`: read what is there,
/// write it plus the line to a temporary file, rename. A target with a
/// second link is refused, so the append never lands elsewhere.
pub fn append_line(paths: &ToolPaths, path: &Path, line: &str) -> io::Result<()> {
    paths
        .check(path, Expect::FileOrAbsent)
        .map_err(io::Error::other)?;
    let mut bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn escaping_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(unescape(&escape(&all)).expect("decodes"), all);
        for sample in [
            &b"plain"[..],
            b"with space",
            b"tab\there",
            b"new\nline",
            b"back\\slash",
            b"\xff\xfe bytes",
            b"",
        ] {
            assert_eq!(
                unescape(&escape(sample)).expect("decodes"),
                sample,
                "{sample:?}"
            );
        }
        assert!(unescape(r"\x4").is_err());
        assert!(unescape(r"\q").is_err());
        assert!(unescape(r"trailing\").is_err());
    }

    #[test]
    fn an_escaped_field_has_no_separators() {
        let encoded = escape(b"a b\tc\nd\re\\f");
        assert!(
            !encoded.contains([' ', '\t', '\n', '\r']),
            "separators survived: {encoded:?}"
        );
    }

    /// A signature line with a name holding spaces and newlines still
    /// splits into four fields, and the fourth decodes to the name.
    #[test]
    fn a_signature_line_splits_back_into_four_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = "with space\nand newline";
        std::fs::write(dir.path().join(name), b"x").expect("write");
        let lines = signature(dir.path()).expect("signature");
        assert_eq!(lines.len(), 1);
        let fields: Vec<&str> = lines[0].split(' ').collect();
        assert_eq!(fields.len(), 4, "{}", lines[0]);
        assert_eq!(fields[0], "file");
        assert_eq!(fields[1], "1");
        assert_eq!(unescape(fields[3]).expect("decodes"), name.as_bytes());
    }

    /// Directories record no size, so a copied tree signs the same as
    /// its source.
    #[test]
    fn a_copied_tree_signs_the_same() {
        let a = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(a.path().join("d/e")).expect("dirs");
        std::fs::write(a.path().join("d/e/f"), b"content").expect("write");
        std::fs::write(a.path().join("g"), b"").expect("write");
        let b = tempfile::tempdir().expect("tempdir");
        super::super::snapshot::copy_tree(a.path(), &b.path().join("copy"), &mut |_| Ok(()))
            .expect("copy");
        assert_eq!(
            signature(a.path()).expect("a"),
            signature(&b.path().join("copy")).expect("b")
        );
        std::fs::write(b.path().join("copy/g"), b"changed").expect("write");
        let d = diff(
            &signature(a.path()).expect("a"),
            &signature(&b.path().join("copy")).expect("b"),
        );
        assert_eq!(d, ["~ g"]);
        assert_eq!(compact_diff(&d), "~g");
        assert_eq!(compact_diff(&[]), "=");
    }

    /// An append never writes in place: a target with a second link is
    /// refused and the other link's content is untouched, and a plain
    /// target is replaced rather than extended.
    #[test]
    fn an_append_never_writes_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ToolPaths::new(dir.path().canonicalize().expect("canonical"));
        std::fs::create_dir(paths.cells()).expect("cells");
        let outside = tempfile::tempdir().expect("tempdir");
        let shared = outside.path().join("shared");
        std::fs::write(&shared, b"original\n").expect("write");
        std::fs::hard_link(&shared, paths.summary()).expect("hard link");
        assert!(append_line(&paths, &paths.summary(), "row").is_err());
        assert_eq!(std::fs::read(&shared).expect("read"), b"original\n");

        std::fs::remove_file(paths.summary()).expect("remove");
        append_line(&paths, &paths.summary(), "first").expect("append");
        let before = std::fs::metadata(paths.summary()).expect("meta").ino();
        append_line(&paths, &paths.summary(), "second").expect("append");
        let after = std::fs::metadata(paths.summary()).expect("meta").ino();
        assert_ne!(
            before, after,
            "an append must replace the file, not extend it"
        );
        assert_eq!(
            std::fs::read_to_string(paths.summary()).expect("read"),
            "first\nsecond\n"
        );
    }
}
