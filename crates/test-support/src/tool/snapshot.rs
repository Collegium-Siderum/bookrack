// SPDX-License-Identifier: Apache-2.0

//! The atomic switch between `host/` and `golden/`, and the cleanup of
//! what an interrupted switch leaves.
//!
//! A snapshot copies `host/` to a temporary tree, checks the copy, and
//! swaps it in as `golden/` through two renames; a restore goes the
//! other way. Any step failing leaves the tree that was there before.

use std::io;
use std::path::{Path, PathBuf};

use super::tripwire::{check_registry, check_tree};
use super::{Expect, Outcome, STAGE_PATHS, STAGE_TRIPWIRE, ToolPaths, ToolResult, problems, sig};

/// Which way the switch goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `host/` → `golden/`.
    Snapshot,
    /// `golden/` → `host/`.
    Restore,
}

/// The points a switch passes, offered to a test's callback so a
/// failure can be injected at each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `files` files have been copied into the temporary tree.
    Copied { files: usize },
    /// The old tree has been moved aside.
    FirstRenamed,
    /// The temporary tree is about to take the old tree's place.
    BeforeSecondRename,
    /// The old tree is about to be removed.
    BeforeCleanup,
}

/// Copy the tree at `src` to `dst`, which must not exist, file by file.
/// A symbolic link, socket or FIFO in the source is an error: a tree to
/// keep must not hold a live endpoint. `on_file` is told the count after
/// each file.
pub fn copy_tree(
    src: &Path,
    dst: &Path,
    on_file: &mut dyn FnMut(usize) -> io::Result<()>,
) -> io::Result<usize> {
    let mut files = 0;
    let mut pending = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from, to)) = pending.pop() {
        let meta = std::fs::symlink_metadata(&from)?;
        let kind = meta.file_type();
        if kind.is_dir() {
            std::fs::create_dir(&to)?;
            for entry in std::fs::read_dir(&from)? {
                let entry = entry?;
                pending.push((entry.path(), to.join(entry.file_name())));
            }
        } else if kind.is_file() {
            std::fs::copy(&from, &to)?;
            files += 1;
            on_file(files)?;
        } else {
            return Err(io::Error::other(format!(
                "{} is not a file or directory; a tree to keep must hold neither links nor endpoints",
                from.display()
            )));
        }
    }
    Ok(files)
}

/// Switch in `direction`, calling `on_step` at each point.
pub fn switch(
    paths: &ToolPaths,
    direction: Direction,
    on_step: &mut dyn FnMut(Step) -> io::Result<()>,
) -> ToolResult<()> {
    let (src, dst) = match direction {
        Direction::Snapshot => (paths.host(), paths.golden()),
        Direction::Restore => (paths.golden(), paths.host()),
    };
    if direction == Direction::Restore && !src.is_dir() {
        return Err(Outcome::refused(
            STAGE_PATHS,
            problems::no_golden(paths.dir()),
        ));
    }
    let refuse = |why: String| Outcome::refused(STAGE_PATHS, problems::path_refused(&why));
    paths.check(&src, Expect::Dir).map_err(refuse)?;
    paths.check(&dst, Expect::DirOrAbsent).map_err(refuse)?;
    let pid = std::process::id();
    let tmp = sibling(&dst, &format!("tmp-{pid}"));
    let old = sibling(&dst, &format!("old-{pid}"));
    let io_err = |what: &str, err: io::Error| Outcome::io(STAGE_PATHS, what, err);

    if let Err(e) = copy_tree(&src, &tmp, &mut |files| on_step(Step::Copied { files })) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(io_err("copy the tree", e));
    }

    // The copy is judged before it goes anywhere: no links, every
    // registry root under the host tree it will be restored to, and the
    // same signature as its source.
    let mut findings = check_tree(&tmp, &mut |_| {});
    let host_root = paths.host();
    findings.extend(check_registry(
        &tmp.join(bookrack_config::DEFAULT_REGISTRY_NAME),
        &host_root,
    ));
    if !findings.is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(Outcome::refused(
            STAGE_TRIPWIRE,
            problems::tripped(&findings),
        ));
    }
    let same = sig::signature(&src).map_err(|e| io_err("sign the source tree", e))?
        == sig::signature(&tmp).map_err(|e| io_err("sign the copied tree", e))?;
    if !same {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(Outcome::tool_error(
            STAGE_PATHS,
            bookrack_core::Problem::new("the copied tree does not match its source")
                .detail("the signatures of the source and the copy differ")
                .hint("something wrote to the tree while it was being copied; retry with nothing running"),
        ));
    }

    if dst.exists() {
        std::fs::rename(&dst, &old).map_err(|e| io_err("move the old tree aside", e))?;
    }
    on_step(Step::FirstRenamed).map_err(|e| io_err("switch the tree", e))?;
    on_step(Step::BeforeSecondRename).map_err(|e| io_err("switch the tree", e))?;
    std::fs::rename(&tmp, &dst).map_err(|e| io_err("move the new tree into place", e))?;
    on_step(Step::BeforeCleanup).map_err(|e| io_err("switch the tree", e))?;
    if old.exists() {
        std::fs::remove_dir_all(&old).map_err(|e| io_err("remove the old tree", e))?;
    }
    Ok(())
}

/// `<dir>/<name>.<suffix>` beside `dir`.
fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.with_file_name(format!("{name}.{suffix}"))
}

/// Put right what an interrupted switch left, for `host` and `golden`:
/// a `*.tmp-*` is removed; a `*.old-*` beside a missing tree is the
/// tree, renamed back; a `*.old-*` beside a present tree is the old
/// tree whose removal failed, removed now. Each action is a note.
pub fn cleanup_residue(paths: &ToolPaths) -> io::Result<Vec<String>> {
    let mut notes = Vec::new();
    let entries = std::fs::read_dir(paths.dir())?;
    let mut leftovers: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        for base in ["host", "golden"] {
            if name.starts_with(&format!("{base}.tmp-"))
                || name.starts_with(&format!("{base}.old-"))
            {
                leftovers.push((base.to_string(), entry.path()));
            }
        }
    }
    leftovers.sort();
    for (base, path) in leftovers {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let tree = paths.dir().join(&base);
        if name.contains(".tmp-") {
            std::fs::remove_dir_all(&path)?;
            notes.push(format!("removed the interrupted copy {name}"));
        } else if tree.exists() {
            std::fs::remove_dir_all(&path)?;
            notes.push(format!(
                "removed the old tree {name} a finished switch left"
            ));
        } else {
            std::fs::rename(&path, &tree)?;
            notes.push(format!(
                "renamed {name} back to {base}: its switch did not finish"
            ));
        }
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Sandbox;

    /// A sandbox directory with a host tree holding one registered root
    /// and a file, snapshotted once.
    fn sandbox() -> (tempfile::TempDir, ToolPaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ToolPaths::new(dir.path().canonicalize().expect("canonical"));
        let host = Sandbox::create_at(&paths.host()).expect("host");
        let alpha = host.data_root("alpha");
        std::fs::write(alpha.join("a.txt"), b"one").expect("write");
        std::fs::write(alpha.join("b.txt"), b"two").expect("write");
        std::fs::write(host.cwd().join("c.txt"), b"three").expect("write");
        host.write_registry_entries(Some("alpha"), &[("alpha", alpha.as_path())]);
        switch(&paths, Direction::Snapshot, &mut |_| Ok(())).expect("first snapshot");
        (dir, paths)
    }

    fn golden_signature(paths: &ToolPaths) -> Vec<String> {
        sig::signature(&paths.golden()).expect("golden signature")
    }

    fn fail_at(step: Step) -> impl FnMut(Step) -> io::Result<()> {
        move |s| {
            if s == step {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        }
    }

    fn residue(paths: &ToolPaths, pattern: &str) -> Vec<String> {
        std::fs::read_dir(paths.dir())
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(pattern))
            .collect()
    }

    #[test]
    fn a_copy_that_fails_midway_keeps_the_previous_golden() {
        let (_dir, paths) = sandbox();
        let before = golden_signature(&paths);
        std::fs::write(paths.host().join("cwd/d.txt"), b"four").expect("write");
        let err = switch(&paths, Direction::Snapshot, &mut |s| match s {
            Step::Copied { files: 3 } => Err(io::Error::other("injected")),
            _ => Ok(()),
        })
        .expect_err("the injected failure");
        assert!(err.problem.is_some());
        assert_eq!(golden_signature(&paths), before, "golden must be untouched");
        let notes = cleanup_residue(&paths).expect("cleanup");
        assert!(
            residue(&paths, ".tmp-").is_empty(),
            "no temporary tree left: {notes:?}"
        );
    }

    #[test]
    fn a_failure_after_the_first_rename_restores_the_old_tree() {
        let (_dir, paths) = sandbox();
        let before = golden_signature(&paths);
        std::fs::write(paths.host().join("cwd/d.txt"), b"four").expect("write");
        switch(
            &paths,
            Direction::Snapshot,
            &mut fail_at(Step::FirstRenamed),
        )
        .expect_err("the injected failure");
        assert!(
            !paths.golden().exists(),
            "golden is moved aside at this point"
        );
        assert_eq!(residue(&paths, "golden.old-").len(), 1);
        let notes = cleanup_residue(&paths).expect("cleanup");
        assert!(notes.iter().any(|n| n.contains("renamed")), "{notes:?}");
        assert_eq!(golden_signature(&paths), before);
        assert!(residue(&paths, ".old-").is_empty() && residue(&paths, ".tmp-").is_empty());
    }

    #[test]
    fn a_failed_second_rename_restores_the_old_tree() {
        let (_dir, paths) = sandbox();
        let before = golden_signature(&paths);
        std::fs::write(paths.host().join("cwd/d.txt"), b"four").expect("write");
        switch(
            &paths,
            Direction::Snapshot,
            &mut fail_at(Step::BeforeSecondRename),
        )
        .expect_err("the injected failure");
        assert!(!paths.golden().exists());
        cleanup_residue(&paths).expect("cleanup");
        assert_eq!(golden_signature(&paths), before);
    }

    #[test]
    fn a_failed_cleanup_keeps_the_new_tree() {
        let (_dir, paths) = sandbox();
        let before = golden_signature(&paths);
        std::fs::write(paths.host().join("cwd/d.txt"), b"four").expect("write");
        switch(
            &paths,
            Direction::Snapshot,
            &mut fail_at(Step::BeforeCleanup),
        )
        .expect_err("the injected failure");
        assert!(paths.golden().exists());
        assert_eq!(residue(&paths, "golden.old-").len(), 1);
        let after = golden_signature(&paths);
        assert_ne!(after, before, "the new tree is in place");
        assert!(after.iter().any(|l| l.ends_with("cwd/d.txt")));
        let notes = cleanup_residue(&paths).expect("cleanup");
        assert!(
            notes.iter().any(|n| n.contains("removed the old tree")),
            "{notes:?}"
        );
        assert!(residue(&paths, ".old-").is_empty());
        assert_eq!(golden_signature(&paths), after);
    }

    /// A restore brings the host back to the golden tree.
    #[test]
    fn a_restore_brings_the_host_back() {
        let (_dir, paths) = sandbox();
        let golden = golden_signature(&paths);
        std::fs::write(paths.host().join("cwd/d.txt"), b"four").expect("write");
        std::fs::remove_file(paths.host().join("cwd/c.txt")).expect("remove");
        assert_ne!(sig::signature(&paths.host()).expect("host"), golden);
        switch(&paths, Direction::Restore, &mut |_| Ok(())).expect("restore");
        assert_eq!(sig::signature(&paths.host()).expect("host"), golden);
    }

    /// A tree with a link in it is not kept.
    #[test]
    fn a_tree_with_a_link_is_not_snapshotted() {
        let (_dir, paths) = sandbox();
        let outside = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(outside.path(), paths.host().join("cwd/out")).expect("symlink");
        let err = switch(&paths, Direction::Snapshot, &mut |_| Ok(())).expect_err("refused");
        assert!(err.problem.is_some());
        assert!(residue(&paths, ".tmp-").is_empty());
    }
}
