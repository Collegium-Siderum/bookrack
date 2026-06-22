// SPDX-License-Identifier: Apache-2.0

//! `bookrack libraries list` — render the registered library entries
//! straight from the on-disk registry.
//!
//! `bookrack libraries fork` — clone a data root into a sibling
//! library with no vector store of its own, so the next
//! `vectors reset` rebuilds the chunks table under a different model
//! without disturbing the source library.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use bookrack_config::Config;

use crate::render;

pub fn list(json: bool) -> Result<()> {
    let entries = bookrack_config::list_libraries().context("list libraries")?;
    if json {
        render::libraries_list_json(entries.as_deref());
    } else {
        render::libraries_list(entries.as_deref());
    }
    Ok(())
}

/// How `fork` brings the envelope store across.
///
/// Hardlinks are the default: they share inodes with the source, so
/// the new library carries no extra envelope bytes. A cross-filesystem
/// target falls back to a byte-for-byte copy automatically. `Copy`
/// forces the byte copy regardless — useful when the caller wants the
/// new library completely independent on disk.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// Hardlink each envelope file; fall back to copy on cross-fs.
    Hardlink,
    /// Copy every envelope file's bytes; never hardlink.
    Copy,
}

/// Render `bookrack libraries fork` — clone the current data root's
/// catalog, corpus, and envelope store into a sibling library. The
/// vector store is NOT carried over; the caller runs `vectors reset`
/// against the new library to rebuild it under the new model.
pub fn fork<F>(
    cfg: &Config,
    new_name: &str,
    target: &Path,
    mode: CopyMode,
    yes: bool,
    ask: F,
) -> Result<()>
where
    F: FnOnce(&str) -> Result<bool>,
{
    validate_inputs(cfg, new_name, target)?;
    let registry_path = registry_target_path()?;

    let source_books = cfg.books_dir();
    let source_catalog = cfg.catalog_db();
    let source_corpus = cfg.corpus_db();
    if !source_catalog.exists() {
        bail!(
            "source catalog.db is missing at {} — fork needs an initialized library",
            source_catalog.display()
        );
    }
    if !source_corpus.exists() {
        bail!(
            "source corpus.db is missing at {} — fork needs an initialized library",
            source_corpus.display()
        );
    }

    let target_books = target.join("books");
    let target_catalog = target.join("catalog.db");
    let target_corpus = target.join("corpus.db");

    println!("fork plan:");
    println!("  source: {}", cfg.data_dir().display());
    println!(
        "  target: {} (library name '{}')",
        target.display(),
        new_name
    );
    println!(
        "  books/      {}",
        match mode {
            CopyMode::Hardlink => "hardlink (falls back to copy on cross-fs)",
            CopyMode::Copy => "copy (force)",
        }
    );
    println!("  catalog.db  copy");
    println!("  corpus.db   copy");
    println!("  lancedb/    NOT carried over (will be rebuilt by `vectors reset`)");
    println!("  sources/    NOT carried over");
    println!("  config.toml NOT carried over");

    if !yes && !ask("Type 'yes' to continue: ")? {
        println!("aborted; no changes written");
        return Ok(());
    }

    std::fs::create_dir_all(target).with_context(|| format!("create {}", target.display()))?;

    if source_books.exists() {
        copy_or_link_envelope_dir(&source_books, &target_books, mode).context("clone books/")?;
    } else {
        // A library with no ingested books has no books/ yet; nothing to copy.
        std::fs::create_dir_all(&target_books)
            .with_context(|| format!("create {}", target_books.display()))?;
    }

    // SQLite's `VACUUM INTO` materializes a consistent snapshot even
    // when the source has uncheckpointed WAL pages, so the fork is safe
    // to run while another process holds a writer. A plain file copy
    // would miss the `-wal`/`-shm` sidecars and risk a torn snapshot.
    bookrack_dbkit::backup_database(&source_catalog, &target_catalog).with_context(|| {
        format!(
            "backup {} -> {}",
            source_catalog.display(),
            target_catalog.display()
        )
    })?;
    bookrack_dbkit::backup_database(&source_corpus, &target_corpus).with_context(|| {
        format!(
            "backup {} -> {}",
            source_corpus.display(),
            target_corpus.display()
        )
    })?;

    bookrack_config::merge_library_into_registry(&registry_path, new_name, target)
        .with_context(|| format!("register '{}' in {}", new_name, registry_path.display()))?;

    println!();
    println!("forked '{}' at {}", new_name, target.display());
    println!("registry updated: {}", registry_path.display());
    println!();
    println!("next steps:");
    println!("  bookrack quit");
    println!("  BOOKRACK_EMBED_MODEL=<new-model> bookrack --library {new_name} run");
    println!("  bookrack vectors reset");
    Ok(())
}

/// Reject the obvious user errors before any filesystem write.
fn validate_inputs(cfg: &Config, new_name: &str, target: &Path) -> Result<()> {
    if new_name.trim().is_empty() {
        bail!("new library name must not be empty");
    }
    if !target.is_absolute() {
        bail!("--data-dir must be an absolute path: {}", target.display());
    }
    let source = cfg.data_dir();
    let source_canonical = source
        .canonicalize()
        .with_context(|| format!("canonicalize source library {}", source.display()))?;
    let target_canonical = canonicalize_target(target).with_context(|| {
        format!(
            "canonicalize target {} (parent must exist before fork runs)",
            target.display()
        )
    })?;
    if target_canonical == source_canonical {
        bail!(
            "target {} resolves to the same path as the source library; fork would be a no-op",
            target.display()
        );
    }
    if let Some(entries) = bookrack_config::list_libraries().context("list libraries")?
        && entries.iter().any(|e| e.name == new_name)
    {
        bail!(
            "registry already has a library named '{}'; choose another name",
            new_name
        );
    }
    if target.exists() {
        // Accept an empty directory so a script that pre-creates the
        // mount point still works. Anything inside is refused.
        let mut iter =
            std::fs::read_dir(target).with_context(|| format!("read {}", target.display()))?;
        if iter.next().is_some() {
            bail!(
                "target {} already exists and is not empty; choose a fresh path or `rm -rf` it first",
                target.display()
            );
        }
    }
    Ok(())
}

/// Canonicalize a fork target. The target may not exist on disk yet
/// «fork creates it», so falls back to canonicalizing the parent and
/// rejoining the requested file name. Both branches collapse symlinks,
/// `.`/`..` segments, and trailing slashes, which is the whole point:
/// the self-clone guard compares the result against the source's
/// canonical form rather than against the raw user input.
fn canonicalize_target(target: &Path) -> Result<PathBuf> {
    if target.exists() {
        return target
            .canonicalize()
            .with_context(|| format!("canonicalize {}", target.display()));
    }
    let parent = target
        .parent()
        .with_context(|| format!("target {} has no parent directory", target.display()))?;
    let file_name = target
        .file_name()
        .with_context(|| format!("target {} has no final path component", target.display()))?;
    let parent_canonical = parent
        .canonicalize()
        .with_context(|| format!("canonicalize parent {}", parent.display()))?;
    Ok(parent_canonical.join(file_name))
}

/// Resolve the registry file to write the new entry into. `BOOKRACK_REGISTRY`
/// wins; otherwise the platform default. The fork command refuses if
/// neither resolves — the caller has no place to record the new entry.
fn registry_target_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(bookrack_config::REGISTRY_ENV) {
        return Ok(PathBuf::from(path));
    }
    bookrack_config::default_registry_path().ok_or_else(|| {
        anyhow!(
            "no registry location: set BOOKRACK_REGISTRY=<path> or ensure the platform config directory is available"
        )
    })
}

/// Walk `src` and either hardlink (with copy fallback) or always copy
/// each regular file into `dst`. The books/ layout is one directory
/// per intake, so a shallow walk over two levels is sufficient.
fn copy_or_link_envelope_dir(src: &Path, dst: &Path, mode: CopyMode) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("create {}", dst.display()))?;
    let entries = std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry in {}", src.display()))?;
        let path = entry.path();
        let file_name = entry.file_name();
        let target = dst.join(&file_name);
        let kind = entry
            .file_type()
            .with_context(|| format!("file type for {}", path.display()))?;
        if kind.is_dir() {
            copy_or_link_envelope_dir(&path, &target, mode)?;
        } else if kind.is_file() {
            transfer_file(&path, &target, mode)?;
        }
        // Symlinks and special files under books/ are unexpected; skip
        // them silently rather than copy semantics no test exercises.
    }
    Ok(())
}

/// Move one file from `src` to `dst` per `mode`, falling back from
/// hardlink to copy when the link crosses a filesystem boundary.
fn transfer_file(src: &Path, dst: &Path, mode: CopyMode) -> Result<()> {
    match mode {
        CopyMode::Hardlink => match std::fs::hard_link(src, dst) {
            Ok(()) => Ok(()),
            Err(e) if is_cross_filesystem(&e) => {
                tracing::warn!(
                    src = %src.display(),
                    dst = %dst.display(),
                    "hardlink across filesystems failed; falling back to copy"
                );
                std::fs::copy(src, dst)
                    .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
                Ok(())
            }
            Err(e) => {
                Err(e).with_context(|| format!("hardlink {} -> {}", src.display(), dst.display()))
            }
        },
        CopyMode::Copy => {
            std::fs::copy(src, dst)
                .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
            Ok(())
        }
    }
}

/// Classify an io::Error as the "this would cross a filesystem" case.
/// `ErrorKind::CrossesDevices` is the canonical variant; older
/// Linux/macOS paths surface it as `Other` with errno EXDEV (18).
fn is_cross_filesystem(e: &std::io::Error) -> bool {
    if matches!(e.kind(), std::io::ErrorKind::CrossesDevices) {
        return true;
    }
    e.raw_os_error() == Some(18)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, content).expect("write");
    }

    #[test]
    fn validate_inputs_rejects_symlink_to_source() {
        // Symlinking the target to the source bypasses a raw `==`
        // comparison; canonicalize_target follows the link, so the
        // guard catches it before any clone runs.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).expect("source");
        let link = dir.path().join("link-to-source");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&source, &link).expect("symlink");

        let cfg = Config::new(source.clone(), "http://localhost:11434".to_string());
        let err =
            validate_inputs(&cfg, "clone", &link).expect_err("symlink to source must be rejected");
        assert!(
            format!("{err:#}").contains("resolves to the same path"),
            "want self-clone bail, got: {err:#}",
        );
    }

    #[test]
    fn validate_inputs_rejects_dot_dot_loop_back_to_source() {
        // `<source>/../source` collapses to `<source>`; the raw
        // PathBuf equality misses it but canonicalize does not.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).expect("source");
        let loopy = source.join("..").join("source");

        let cfg = Config::new(source.clone(), "http://localhost:11434".to_string());
        let err = validate_inputs(&cfg, "clone", &loopy).expect_err("dotdot loop must be rejected");
        assert!(
            format!("{err:#}").contains("resolves to the same path"),
            "want self-clone bail, got: {err:#}",
        );
    }

    #[test]
    fn validate_inputs_accepts_distinct_sibling_directory() {
        // A genuine sibling target — does not yet exist on disk — must
        // pass the guard. canonicalize_target falls back to canonicalizing
        // the parent and rejoining the file name.
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).expect("source");
        let target = dir.path().join("clone");

        let cfg = Config::new(source.clone(), "http://localhost:11434".to_string());
        validate_inputs(&cfg, "clone", &target).expect("sibling target accepted");
    }

    #[test]
    fn copy_or_link_envelope_dir_shares_inodes_on_hardlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("books");
        let dst = dir.path().join("books-target");
        touch(&src.join("1").join("envelope.json"), b"book one");
        touch(&src.join("2").join("envelope.json"), b"book two");

        copy_or_link_envelope_dir(&src, &dst, CopyMode::Hardlink).expect("link");

        // Same content.
        assert_eq!(
            fs::read(dst.join("1").join("envelope.json")).expect("read"),
            b"book one"
        );
        assert_eq!(
            fs::read(dst.join("2").join("envelope.json")).expect("read"),
            b"book two"
        );

        // Inode is shared so the target reflects a later write to src.
        // (Skipping ino assertions to stay cross-platform — read-after-write
        // through one side is the user-observable invariant.)
        fs::write(src.join("1").join("envelope.json"), b"updated").expect("update src");
        assert_eq!(
            fs::read(dst.join("1").join("envelope.json")).expect("read"),
            b"updated",
            "hardlinked file must reflect the source's update"
        );
    }

    #[test]
    fn copy_or_link_envelope_dir_with_copy_mode_decouples_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("books");
        let dst = dir.path().join("books-target");
        touch(&src.join("1").join("envelope.json"), b"book one");

        copy_or_link_envelope_dir(&src, &dst, CopyMode::Copy).expect("copy");

        // Modifying the source must NOT affect the target.
        fs::write(src.join("1").join("envelope.json"), b"updated").expect("update src");
        assert_eq!(
            fs::read(dst.join("1").join("envelope.json")).expect("read"),
            b"book one",
            "copy mode must produce an independent file"
        );
    }

    #[test]
    fn copy_or_link_envelope_dir_handles_empty_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("books");
        let dst = dir.path().join("books-target");
        fs::create_dir_all(&src).expect("create empty src");

        copy_or_link_envelope_dir(&src, &dst, CopyMode::Hardlink).expect("empty");

        assert!(dst.exists());
        assert!(
            fs::read_dir(&dst).expect("read dst").next().is_none(),
            "target must be empty"
        );
    }
}
