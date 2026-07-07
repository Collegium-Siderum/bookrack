// SPDX-License-Identifier: Apache-2.0

//! Read-only detection of bookrack data roots.
//!
//! Two questions this module answers, both without touching a daemon or
//! mutating anything on disk:
//!
//! - `detect_library`: does a single path look like a bookrack data
//!   root, and how confident are we?
//! - `scan_for_libraries`: which subdirectories of a set of roots are
//!   data roots?
//!
//! The identity manifest ([`crate::load_manifest`]) is the only
//! confirming evidence. When a root predates the manifest, a strict
//! heuristic — the `catalog.db` + `corpus.db` pair — marks it
//! *probable*; every weaker set of signals is *not a library*, because
//! a false positive (registering an ordinary project directory) costs
//! more than a false negative (which `add` can override by hand). A
//! manifest that exists but cannot be read (foreign magic, a future
//! schema version) is reported as its own verdict, distinct from "not a
//! library", so mount-health tooling can tell "nothing here" apart from
//! "something here I cannot read".

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{LibraryManifest, ROOT_CONFIG_NAME, load_manifest};

/// One piece of heuristic evidence found beside a candidate path. Names
/// map to the on-disk layout a [`crate::Config`] lays down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// `catalog.db` — the book catalog database.
    CatalogDb,
    /// `corpus.db` — the book corpus (chunk text) database.
    CorpusDb,
    /// `papers_catalog.db` — the papers catalog database.
    PapersCatalogDb,
    /// `papers_corpus.db` — the papers corpus database.
    PapersCorpusDb,
    /// `config.toml` — the root config. Weak on its own: ordinary
    /// project directories carry a `config.toml` too.
    RootConfig,
    /// `lancedb/` — the book vector store directory.
    LancedbDir,
    /// `lancedb_papers/` — the papers vector store directory.
    PapersLancedbDir,
    /// `books/` — the envelope store directory.
    BooksDir,
    /// `sources/` — the ingested source files directory.
    SourcesDir,
}

impl Signal {
    /// The on-disk file or directory name this signal corresponds to.
    pub fn filename(self) -> &'static str {
        match self {
            Signal::CatalogDb => "catalog.db",
            Signal::CorpusDb => "corpus.db",
            Signal::PapersCatalogDb => "papers_catalog.db",
            Signal::PapersCorpusDb => "papers_corpus.db",
            Signal::RootConfig => ROOT_CONFIG_NAME,
            Signal::LancedbDir => "lancedb",
            Signal::PapersLancedbDir => "lancedb_papers",
            Signal::BooksDir => "books",
            Signal::SourcesDir => "sources",
        }
    }
}

/// How much a path looks like a bookrack data root.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum DetectVerdict {
    /// A valid identity manifest was read: the path is a bookrack data
    /// root beyond doubt.
    Confirmed(LibraryManifest),
    /// No manifest, but the strict heuristic (`catalog.db` +
    /// `corpus.db`) matched. The collected signals are carried for
    /// display.
    Probable { signals: Vec<Signal> },
    /// A `bookrack-library.toml` is present but could not be read as a
    /// v1 manifest — foreign magic or a future schema version. Distinct
    /// from `NotALibrary` so an unreadable root is not mistaken for an
    /// empty one.
    Unreadable { reason: String },
    /// Neither a manifest nor the heuristic pair. Any signals found are
    /// carried so a human can see what was there.
    NotALibrary { signals: Vec<Signal> },
}

/// Why a single-path detection could not even be attempted. These are
/// caller-input faults, not verdicts about the path's contents.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    /// The path does not exist.
    #[error("{} does not exist", .0.display())]
    NotFound(PathBuf),
    /// The path exists but is not a directory.
    #[error("{} is not a directory", .0.display())]
    NotADirectory(PathBuf),
}

/// Probe whether `path` is a bookrack data root, read-only.
///
/// A missing or non-directory path is a [`DetectError`] (caller input).
/// Everything else resolves to a [`DetectVerdict`]: a readable manifest
/// confirms, an unreadable manifest is reported as such, and a
/// manifest-less directory is judged by the `catalog.db` + `corpus.db`
/// heuristic.
pub fn detect_library(path: &Path) -> Result<DetectVerdict, DetectError> {
    if !path.exists() {
        return Err(DetectError::NotFound(path.to_path_buf()));
    }
    if !path.is_dir() {
        return Err(DetectError::NotADirectory(path.to_path_buf()));
    }
    match load_manifest(path) {
        Ok(Some(manifest)) => Ok(DetectVerdict::Confirmed(manifest)),
        Err(e) => Ok(DetectVerdict::Unreadable {
            reason: e.to_string(),
        }),
        Ok(None) => {
            let signals = collect_signals(path);
            if signals.contains(&Signal::CatalogDb) && signals.contains(&Signal::CorpusDb) {
                Ok(DetectVerdict::Probable { signals })
            } else {
                // A papers-only legacy root (papers_catalog.db +
                // papers_corpus.db, no book catalog) falls here: the
                // heuristic deliberately keys on the book pair only, so
                // such a root reads as not-a-library and must be brought
                // in with an explicit `add`. Confirmed roots are
                // unaffected — they carry a manifest.
                Ok(DetectVerdict::NotALibrary { signals })
            }
        }
    }
}

/// Collect every heuristic signal present beside `path`, in a fixed
/// display order. Existence is the signal; the kind (file vs directory)
/// is not checked beyond what the layout implies.
fn collect_signals(path: &Path) -> Vec<Signal> {
    const CHECKS: [(&str, Signal); 9] = [
        ("catalog.db", Signal::CatalogDb),
        ("corpus.db", Signal::CorpusDb),
        ("papers_catalog.db", Signal::PapersCatalogDb),
        ("papers_corpus.db", Signal::PapersCorpusDb),
        (ROOT_CONFIG_NAME, Signal::RootConfig),
        ("lancedb", Signal::LancedbDir),
        ("lancedb_papers", Signal::PapersLancedbDir),
        ("books", Signal::BooksDir),
        ("sources", Signal::SourcesDir),
    ];
    CHECKS
        .iter()
        .filter(|(name, _)| path.join(name).exists())
        .map(|(_, signal)| *signal)
        .collect()
}

/// The result of a scan: the data roots found, and how many
/// subdirectories were skipped because they could not be read or held
/// an unreadable manifest.
#[derive(Debug, Default)]
pub struct ScanOutcome {
    /// Paths that detected as [`DetectVerdict::Confirmed`] or
    /// [`DetectVerdict::Probable`], each with its verdict.
    pub found: Vec<(PathBuf, DetectVerdict)>,
    /// Count of directories that could not be listed or whose manifest
    /// was unreadable — surfaced so a scan never silently under-reports.
    pub skipped: usize,
}

/// Walk each root up to `depth` directory levels deep and collect the
/// data roots found. A directory that detects as a library is not
/// descended into (a library's own subdirectories are never libraries).
/// `depth` is the number of levels below each root to descend: `1`
/// probes a parent's immediate children, `2` a mounted volume and one
/// level within it.
pub fn scan_for_libraries(roots: &[PathBuf], depth: u8) -> ScanOutcome {
    let mut outcome = ScanOutcome::default();
    for root in roots {
        probe_tree(root, depth, &mut outcome);
    }
    outcome
}

/// Detect `dir`; on a hit, record it and stop. On a miss, descend into
/// its child directories while `depth` levels remain.
fn probe_tree(dir: &Path, depth: u8, outcome: &mut ScanOutcome) {
    match detect_library(dir) {
        Ok(verdict @ (DetectVerdict::Confirmed(_) | DetectVerdict::Probable { .. })) => {
            outcome.found.push((dir.to_path_buf(), verdict));
            return;
        }
        Ok(DetectVerdict::Unreadable { .. }) => {
            outcome.skipped += 1;
            return;
        }
        Ok(DetectVerdict::NotALibrary { .. }) => {}
        Err(_) => {
            // The path vanished or turned non-directory mid-walk; count
            // it as skipped rather than aborting the whole scan.
            outcome.skipped += 1;
            return;
        }
    }
    if depth == 0 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            outcome.skipped += 1;
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            outcome.skipped += 1;
            continue;
        };
        let child = entry.path();
        if child.is_dir() {
            probe_tree(&child, depth - 1, outcome);
        }
    }
}

/// The mounted volumes to scan under `--volumes`, best-effort per
/// platform. On macOS this is the entries under `/Volumes`; on Linux the
/// real (non-pseudo) mount points from `/proc/mounts`; elsewhere empty.
#[cfg(target_os = "macos")]
pub fn mounted_volumes() -> Vec<PathBuf> {
    let mut vols = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/Volumes") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                vols.push(path);
            }
        }
    }
    vols
}

/// See [`mounted_volumes`]. Linux reads `/proc/mounts` and drops
/// pseudo-filesystems.
#[cfg(target_os = "linux")]
pub fn mounted_volumes() -> Vec<PathBuf> {
    match std::fs::read_to_string("/proc/mounts") {
        Ok(content) => parse_proc_mounts(&content),
        Err(_) => Vec::new(),
    }
}

/// See [`mounted_volumes`]. Platforms other than macOS and Linux have no
/// volume enumeration, so `--volumes` finds nothing.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn mounted_volumes() -> Vec<PathBuf> {
    Vec::new()
}

/// Parse `/proc/mounts` content into the real mount points, dropping
/// pseudo-filesystems by type and by the conventional virtual-tree
/// prefixes. Factored out so it is testable without the real file.
#[cfg(target_os = "linux")]
fn parse_proc_mounts(content: &str) -> Vec<PathBuf> {
    const PSEUDO_FSTYPES: &[&str] = &[
        "proc",
        "sysfs",
        "devtmpfs",
        "devpts",
        "tmpfs",
        "cgroup",
        "cgroup2",
        "pstore",
        "bpf",
        "tracefs",
        "debugfs",
        "mqueue",
        "hugetlbfs",
        "securityfs",
        "configfs",
        "fusectl",
        "autofs",
        "binfmt_misc",
        "ramfs",
    ];
    const PSEUDO_PREFIXES: &[&str] = &["/proc", "/sys", "/dev", "/run"];
    let mut mounts = Vec::new();
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let Some(_device) = fields.next() else {
            continue;
        };
        let Some(mount_point) = fields.next() else {
            continue;
        };
        let Some(fstype) = fields.next() else {
            continue;
        };
        if PSEUDO_FSTYPES.contains(&fstype) {
            continue;
        }
        if PSEUDO_PREFIXES
            .iter()
            .any(|p| mount_point == *p || mount_point.starts_with(&format!("{p}/")))
        {
            continue;
        }
        mounts.push(PathBuf::from(mount_point));
    }
    mounts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LibraryKind, new_manifest, write_manifest};

    fn touch(path: &Path) {
        std::fs::write(path, b"").expect("touch");
    }

    #[test]
    fn a_manifest_confirms() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = new_manifest("lib", LibraryKind::Prod, None);
        write_manifest(dir.path(), &manifest).expect("write");
        match detect_library(dir.path()).expect("detect") {
            DetectVerdict::Confirmed(m) => assert_eq!(m.uuid, manifest.uuid),
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn catalog_and_corpus_pair_is_probable() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(&dir.path().join("catalog.db"));
        touch(&dir.path().join("corpus.db"));
        match detect_library(dir.path()).expect("detect") {
            DetectVerdict::Probable { signals } => {
                assert!(signals.contains(&Signal::CatalogDb));
                assert!(signals.contains(&Signal::CorpusDb));
            }
            other => panic!("expected Probable, got {other:?}"),
        }
    }

    #[test]
    fn a_lone_config_toml_is_not_a_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(&dir.path().join(ROOT_CONFIG_NAME));
        match detect_library(dir.path()).expect("detect") {
            DetectVerdict::NotALibrary { signals } => {
                assert_eq!(signals, vec![Signal::RootConfig]);
            }
            other => panic!("expected NotALibrary, got {other:?}"),
        }
    }

    #[test]
    fn books_and_sources_alone_are_not_a_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("books")).expect("books");
        std::fs::create_dir(dir.path().join("sources")).expect("sources");
        assert!(matches!(
            detect_library(dir.path()).expect("detect"),
            DetectVerdict::NotALibrary { .. }
        ));
    }

    #[test]
    fn a_papers_only_root_is_not_a_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(&dir.path().join("papers_catalog.db"));
        touch(&dir.path().join("papers_corpus.db"));
        assert!(matches!(
            detect_library(dir.path()).expect("detect"),
            DetectVerdict::NotALibrary { .. }
        ));
    }

    #[test]
    fn a_bad_magic_manifest_is_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(crate::MANIFEST_FILENAME),
            "format = \"something-else\"\n",
        )
        .expect("seed");
        assert!(matches!(
            detect_library(dir.path()).expect("detect"),
            DetectVerdict::Unreadable { .. }
        ));
    }

    #[test]
    fn a_future_schema_version_is_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(crate::MANIFEST_FILENAME),
            "format = \"bookrack-library\"\nformat_version = 99\nuuid = \"u\"\nname = \"n\"\n",
        )
        .expect("seed");
        assert!(matches!(
            detect_library(dir.path()).expect("detect"),
            DetectVerdict::Unreadable { .. }
        ));
    }

    #[test]
    fn a_missing_path_is_a_detect_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            detect_library(&dir.path().join("nope")),
            Err(DetectError::NotFound(_))
        ));
    }

    #[test]
    fn a_file_path_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f");
        touch(&file);
        assert!(matches!(
            detect_library(&file),
            Err(DetectError::NotADirectory(_))
        ));
    }

    #[test]
    fn scan_finds_children_and_prunes_at_a_hit() {
        let root = tempfile::tempdir().expect("tempdir");
        // A confirmed library one level down.
        let lib = root.path().join("lib-a");
        std::fs::create_dir(&lib).expect("lib");
        write_manifest(&lib, &new_manifest("a", LibraryKind::Prod, None)).expect("manifest");
        // A nested subdirectory of the library that itself looks like a
        // library must not be reported once the parent is a hit.
        let nested = lib.join("inner");
        std::fs::create_dir(&nested).expect("nested");
        touch(&nested.join("catalog.db"));
        touch(&nested.join("corpus.db"));
        // A plain directory that is not a library.
        std::fs::create_dir(root.path().join("not-a-lib")).expect("plain");

        let outcome = scan_for_libraries(&[root.path().to_path_buf()], 1);
        assert_eq!(outcome.found.len(), 1, "only the top-level library");
        assert_eq!(outcome.found[0].0, lib);
    }

    #[test]
    fn scan_counts_unreadable_subdirectories() {
        let root = tempfile::tempdir().expect("tempdir");
        let bad = root.path().join("bad");
        std::fs::create_dir(&bad).expect("bad");
        std::fs::write(bad.join(crate::MANIFEST_FILENAME), "format = \"nope\"\n").expect("seed");

        let outcome = scan_for_libraries(&[root.path().to_path_buf()], 1);
        assert_eq!(outcome.found.len(), 0);
        assert_eq!(outcome.skipped, 1);
    }

    #[test]
    fn detect_verdict_serializes_with_a_tag() {
        let json = serde_json::to_value(DetectVerdict::Probable {
            signals: vec![Signal::CatalogDb, Signal::CorpusDb],
        })
        .expect("serialize");
        assert_eq!(json["verdict"], "probable");
        assert_eq!(json["signals"][0], "catalog_db");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_mounts_drops_pseudo_filesystems() {
        let content = "\
proc /proc proc rw,nosuid 0 0
sysfs /sys sysfs rw,nosuid 0 0
/dev/sda1 / ext4 rw,relatime 0 0
tmpfs /run tmpfs rw,nosuid 0 0
/dev/sdb1 /mnt/external ext4 rw,relatime 0 0
";
        let mounts = parse_proc_mounts(content);
        assert_eq!(
            mounts,
            vec![PathBuf::from("/"), PathBuf::from("/mnt/external")]
        );
    }
}
