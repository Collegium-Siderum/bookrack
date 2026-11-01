// SPDX-License-Identifier: Apache-2.0

//! Reading the workspace's own source, for the gates that hold a
//! judgement to a marker line written next to the code it judges.

use std::path::{Path, PathBuf};

use eyre::Result;

/// Every member crate, by directory name.
///
/// Read off the filesystem rather than held as a list: a list is a
/// second place to remember, and the failure it produces — a crate
/// nobody added — is silent.
pub(crate) fn member_crates(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root.join("crates"))? {
        let path = entry?.path();
        if !path.join("src").is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// The workspace root, from this crate's manifest directory.
pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> always has two ancestors")
        .to_path_buf()
}

/// Every `.rs` file under a crate's `src/`, in no particular order.
pub(crate) fn rust_sources(crate_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![crate_dir.join("src")];
    while let Some(dir) = stack.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// Where a file's test module begins, or its length when it has none.
///
/// `#[cfg(test)]` alone does not mark it: the attribute also gates
/// imports a test-only derive needs, and cutting at the first one of
/// those truncates a file at its `use` block and hides everything
/// below. Only the attribute standing on a `mod` starts the tests.
pub(crate) fn test_module_start(lines: &[&str]) -> usize {
    for (idx, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let opens_a_module = lines[idx + 1..]
            .iter()
            .find(|next| !next.trim().is_empty())
            .is_some_and(|next| {
                let next = next.trim();
                next.starts_with("mod ") || next.starts_with("pub mod ")
            });
        if opens_a_module {
            return idx;
        }
    }
    lines.len()
}

/// The payload of the `marker` line attached to line `idx`.
///
/// The marker sits on the line directly above, attributes skipped —
/// below a doc comment, which describes the item, rather than above
/// it, which would put a machine-readable line where a reader expects
/// prose.
pub(crate) fn marker_above(lines: &[&str], idx: usize, marker: &str) -> Option<String> {
    let mut cursor = idx;
    while cursor > 0 {
        cursor -= 1;
        let line = lines[cursor].trim();
        if line.starts_with('#') {
            continue;
        }
        return line
            .strip_prefix(marker)
            .map(|payload| payload.trim().to_string());
    }
    None
}
