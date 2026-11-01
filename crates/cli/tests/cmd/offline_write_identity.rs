// SPDX-License-Identifier: Apache-2.0

//! Commands that write to a library from the CLI process, with no
//! daemon in between, check the data root's identity before they touch
//! it. A registry entry that caches a uuid names a root whose manifest
//! records that uuid; a root that carries no manifest, or one recording
//! another uuid, is refused with the reason the daemon gives for the
//! same root, closed with running the command again, and left exactly
//! as it was found.
//!
//! Every test registers `alpha` and `beta`, then stages alpha's root in
//! one of four shapes: an empty directory (the mount point of a volume
//! that is not connected), no directory at all, a root holding data
//! under beta's manifest, or a root holding data with no manifest.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use bookrack_config::MANIFEST_FILENAME;
use bookrack_corpus::{Corpus, EMBED_MODEL_KEY};
use bookrack_test_support::fixtures::{DISTILL_TINY_BOOK_TOML, DISTILL_TINY_SOURCE};
use bookrack_test_support::tool::sig::signature;
use bookrack_test_support::{Sandbox, bookrack_cmd};

/// Two registered libraries in one sandbox, each root carrying the
/// manifest its entry caches the uuid of.
struct World {
    sandbox: Sandbox,
    alpha: PathBuf,
    beta: PathBuf,
}

/// Which of the two refusals a staged root earns.
#[derive(Debug, Clone, Copy)]
enum Fault {
    /// The root carries no manifest.
    NoManifest,
    /// The root's manifest records another uuid.
    AnotherUuid,
}

impl Fault {
    fn summary(self, library: &str) -> String {
        match self {
            Fault::NoManifest => format!(
                "cannot write to library '{library}': its data root carries no identity manifest"
            ),
            Fault::AnotherUuid => format!(
                "cannot write to library '{library}': the identity manifest at its data root \
                 records another uuid"
            ),
        }
    }
}

/// Everything a refused command must leave as it was: alpha's tree,
/// or its absence, beta's tree, and the registry file.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    alpha: Option<Vec<String>>,
    beta: Vec<String>,
    registry: Vec<u8>,
}

impl World {
    fn new() -> World {
        let sandbox = Sandbox::new();
        let alpha = sandbox.data_root("alpha");
        let beta = sandbox.data_root("beta");
        let world = World {
            sandbox,
            alpha,
            beta,
        };
        for (name, root) in [("alpha", &world.alpha), ("beta", &world.beta)] {
            let out = world.run(&[
                "libraries",
                "add",
                name,
                path(root),
                "--yes",
                "--kind",
                "test",
            ]);
            assert_success(&out, "libraries add");
        }
        world
    }

    /// Run `bookrack` with `args`, the library chosen by the arguments
    /// alone.
    fn run(&self, args: &[&str]) -> Output {
        bookrack_cmd!(&self.sandbox)
            .without_data_dir()
            .build()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run bookrack")
    }

    /// Alpha's root as an empty directory.
    fn empty_alpha(&self) {
        fs::remove_dir_all(&self.alpha).expect("remove alpha");
        fs::create_dir(&self.alpha).expect("recreate alpha");
    }

    /// No directory at alpha's root.
    fn missing_alpha(&self) {
        fs::remove_dir_all(&self.alpha).expect("remove alpha");
    }

    /// Alpha's root holding a distill build's stores under beta's
    /// manifest: beta's identity, registered at beta's own root.
    fn foreign_alpha(&self) {
        self.distill_into_alpha();
        fs::copy(
            self.beta.join(MANIFEST_FILENAME),
            self.alpha.join(MANIFEST_FILENAME),
        )
        .expect("copy beta's manifest over alpha's");
    }

    /// Alpha's root holding a catalog and a stamped corpus, with its
    /// manifest gone: a probable data root by its stores, and one whose
    /// index a profile would plan work for.
    fn unidentified_alpha(&self) {
        self.distill_into_alpha();
        let corpus = Corpus::open(&self.alpha.join("corpus.db")).expect("open corpus");
        corpus
            .meta_set(EMBED_MODEL_KEY, "another-embed-model")
            .expect("stamp corpus");
        drop(corpus);
        fs::remove_file(self.alpha.join(MANIFEST_FILENAME)).expect("remove manifest");
    }

    fn distill_into_alpha(&self) {
        let out = self.run(&[
            "--library",
            "alpha",
            "distill",
            "build",
            path(&self.tiny_book()),
        ]);
        assert_success(&out, "distill build into alpha");
    }

    /// A one-page reference book for `distill build`.
    fn tiny_book(&self) -> PathBuf {
        let dir = self.sandbox.inputs().join("tiny");
        fs::create_dir_all(&dir).expect("mkdir tiny");
        fs::write(dir.join("book.toml"), DISTILL_TINY_BOOK_TOML).expect("write book.toml");
        fs::write(dir.join("source.md"), DISTILL_TINY_SOURCE).expect("write source.md");
        dir
    }

    fn snapshot(&self) -> Snapshot {
        let alpha = self
            .alpha
            .exists()
            .then(|| signature(&self.alpha).expect("sign alpha"));
        Snapshot {
            alpha,
            beta: signature(&self.beta).expect("sign beta"),
            registry: fs::read(self.sandbox.registry_path()).expect("read registry"),
        }
    }

    /// `out` is the identity refusal for `library` with `fault`, and
    /// nothing `before` recorded has changed.
    fn assert_refused(&self, before: &Snapshot, out: &Output, library: &str, fault: Fault) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "exit code; stderr:\n{stderr}");
        let first = stderr.lines().next().unwrap_or_default();
        assert_eq!(
            first,
            format!("bookrack: {}", fault.summary(library)),
            "summary; stderr:\n{stderr}"
        );
        let hint = stderr
            .lines()
            .find_map(|line| line.strip_prefix("  hint: "))
            .unwrap_or_else(|| panic!("no hint line; stderr:\n{stderr}"));
        assert!(
            hint.ends_with(". Then run this command again."),
            "the hint closes with the retry: {hint}"
        );
        assert_eq!(
            hint.matches("Then run this command again.").count(),
            1,
            "the retry is said once: {hint}"
        );
        assert!(
            !stderr.contains("libraries mount"),
            "no daemon is involved, so no mount step: {stderr}"
        );
        assert_eq!(
            &self.snapshot(),
            before,
            "a refused command leaves the roots and the registry as they were; stderr:\n{stderr}"
        );
    }
}

fn path(p: &Path) -> &str {
    p.to_str().expect("utf-8 sandbox path")
}

fn assert_success(out: &Output, what: &str) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{what}; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

const PURGE_ALPHA: &[&str] = &["libraries", "remove", "alpha", "--purge", "--yes"];

#[test]
fn purge_refuses_an_empty_mount_point_and_leaves_no_lock() {
    let world = World::new();
    world.empty_alpha();
    let before = world.snapshot();
    let out = world.run(PURGE_ALPHA);
    world.assert_refused(&before, &out, "alpha", Fault::NoManifest);
}

#[test]
fn purge_refuses_another_librarys_root() {
    let world = World::new();
    world.foreign_alpha();
    let before = world.snapshot();
    let out = world.run(PURGE_ALPHA);
    world.assert_refused(&before, &out, "alpha", Fault::AnotherUuid);
}

#[test]
fn purge_refuses_a_root_whose_manifest_is_gone() {
    let world = World::new();
    world.unidentified_alpha();
    let before = world.snapshot();
    let out = world.run(PURGE_ALPHA);
    world.assert_refused(&before, &out, "alpha", Fault::NoManifest);
}

#[test]
fn purge_refuses_a_missing_root_before_locking() {
    let world = World::new();
    world.missing_alpha();
    let before = world.snapshot();
    let out = world.run(PURGE_ALPHA);
    world.assert_refused(&before, &out, "alpha", Fault::NoManifest);
}

#[test]
fn purge_still_deletes_a_root_that_carries_its_identity() {
    let world = World::new();
    let out = world.run(PURGE_ALPHA);
    assert_success(&out, "purge alpha");
    assert!(!world.alpha.exists(), "the purge deletes alpha's root");
    assert!(world.beta.exists(), "beta's root is untouched");
    let registry = fs::read_to_string(world.sandbox.registry_path()).expect("read registry");
    assert!(
        !registry.contains("[libraries.alpha]"),
        "alpha leaves the registry: {registry}"
    );
}
