// SPDX-License-Identifier: Apache-2.0

//! A release golden against the real `bookrack` binary: a rich sandbox
//! freezes, the frozen golden imports into another sandbox that passes
//! the acceptance check and serves a daemon, and every way the import
//! can be wrong is refused before anything is created or cleared: a
//! golden that changed since the freeze, a target path TOML and JSON
//! would escape, an old path outside the rewritten files, overlapping
//! paths, and a source that changes while it is copied.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_test_support::bookrack_bin;
use bookrack_test_support::tool::exec::{DaemonStart, ExecOpts};
use bookrack_test_support::tool::release::RELEASE_RECORD;
use bookrack_test_support::tool::{
    Class, CreateHooks, CreateOpts, Marker, Outcome, Preset, Record, STAGE_PATHS, STAGE_PRESET,
    Session, SessionOpts, Source, sig,
};

fn opts(ws: &Path, replace: bool) -> CreateOpts {
    CreateOpts {
        replace,
        workspace: Some(ws.to_path_buf()),
    }
}

/// A rich sandbox at `dir`, built from a workspace directory `ws`.
async fn build_rich(dir: &Path, ws: &Path) -> Session {
    std::fs::create_dir_all(ws).expect("ws");
    Session::create(
        dir,
        Source::Prebuilt(bookrack_bin!()),
        Preset::Rich,
        opts(ws, false),
        SessionOpts::default(),
    )
    .await
    .unwrap_or_else(|o| panic!("create rich: {o:?}"))
}

/// A bare sandbox at `dir`.
async fn build_bare(dir: &Path, ws: &Path) -> Session {
    std::fs::create_dir_all(ws).expect("ws");
    Session::create(
        dir,
        Source::Prebuilt(bookrack_bin!()),
        Preset::Bare,
        opts(ws, false),
        SessionOpts::default(),
    )
    .await
    .unwrap_or_else(|o| panic!("create bare: {o:?}"))
}

/// Freeze the sandbox at `dir` and return its canonical path.
async fn freeze(dir: &Path) -> PathBuf {
    let mut session = Session::open_for_freeze(dir, SessionOpts::default())
        .unwrap_or_else(|o| panic!("open for freeze: {o:?}"));
    session
        .freeze()
        .await
        .unwrap_or_else(|o| panic!("freeze: {o:?}"));
    dir.canonicalize().expect("canonical")
}

/// Import the frozen sandbox `from` into `dir`.
async fn import(dir: &Path, ws: &Path, from: &Path, replace: bool) -> Result<Session, Outcome> {
    import_with(dir, ws, from, replace, CreateHooks::default()).await
}

async fn import_with(
    dir: &Path,
    ws: &Path,
    from: &Path,
    replace: bool,
    hooks: CreateHooks,
) -> Result<Session, Outcome> {
    std::fs::create_dir_all(ws).expect("ws");
    Session::create_with(
        dir,
        Source::Prebuilt(bookrack_bin!()),
        Preset::Release {
            from: from.to_path_buf(),
        },
        opts(ws, replace),
        SessionOpts::default(),
        hooks,
    )
    .await
}

fn records(dir: &Path) -> Vec<Record> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir.join(".calls"))
        .expect(".calls")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.join("result.json").is_file())
        .collect();
    entries.sort();
    entries
        .iter()
        .map(|p| {
            serde_json::from_slice(&std::fs::read(p.join("result.json")).expect("result.json"))
                .expect("a record")
        })
        .collect()
}

fn marker(dir: &Path) -> Marker {
    serde_json::from_slice(&std::fs::read(dir.join(".sandbox.json")).expect("marker"))
        .expect("a marker")
}

fn signature(path: &Path) -> Vec<String> {
    sig::signature(path).unwrap_or_else(|e| panic!("sign {}: {e}", path.display()))
}

/// Every file and directory under `root` has no write bit.
fn read_only(root: &Path) -> bool {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path).expect("metadata");
        if meta.permissions().mode() & 0o222 != 0 {
            return false;
        }
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).expect("read_dir") {
                pending.push(entry.expect("entry").path());
            }
        }
    }
    true
}

/// Every file under `root` has its owner's write bit.
fn owner_writable(root: &Path) -> bool {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path).expect("metadata");
        if meta.permissions().mode() & 0o200 == 0 {
            return false;
        }
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).expect("read_dir") {
                pending.push(entry.expect("entry").path());
            }
        }
    }
    true
}

fn summary(outcome: &Outcome) -> String {
    outcome
        .problem
        .as_ref()
        .map(|p| p.summary.clone())
        .unwrap_or_default()
}

fn detail(outcome: &Outcome) -> String {
    outcome
        .problem
        .as_ref()
        .and_then(|p| p.data.detail.clone())
        .unwrap_or_default()
}

/// The modes of every path under `root`, for an "unchanged" check that
/// a signature alone would not make.
fn modes(root: &Path) -> Vec<(PathBuf, u32)> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path).expect("metadata");
        out.push((path.clone(), meta.permissions().mode()));
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).expect("read_dir") {
                pending.push(entry.expect("entry").path());
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn a_frozen_golden_imports_and_passes_acceptance() {
    let root = tempfile::tempdir().expect("tempdir");
    let a = root.path().join("a");
    drop(build_rich(&a, &root.path().join("ws-a")).await);
    let a = freeze(&a).await;
    assert!(read_only(&a.join("golden")), "golden/ is read-only");
    assert!(read_only(&a.join(RELEASE_RECORD)));
    assert!(marker(&a).frozen);
    assert!(
        records(&a).iter().any(|r| r.command == "freeze"),
        "the freeze is recorded"
    );
    let a_sig = signature(&a.join("golden"));
    let a_modes = modes(&a.join("golden"));

    // The frozen sandbox refuses to be rebuilt, and writes no record.
    let before = records(&a).len();
    let refused = Session::create(
        &a,
        Source::Prebuilt(bookrack_bin!()),
        Preset::Rich,
        opts(&root.path().join("ws-a"), true),
        SessionOpts::default(),
    )
    .await
    .err()
    .expect("a frozen sandbox refuses --replace");
    assert_eq!(refused.stage, STAGE_PATHS, "{refused:?}");
    assert_eq!(summary(&refused), "the sandbox is frozen");
    assert_eq!(records(&a).len(), before, "the refusal writes no record");

    // The import, to a path of another length.
    let b = root.path().join("b2");
    let mut session = import(&b, &root.path().join("ws-b"), &a, false)
        .await
        .unwrap_or_else(|o| panic!("import: {o:?}"));
    let b = session.paths().dir().to_path_buf();
    let registry = std::fs::read_to_string(b.join("golden/registry.toml")).expect("registry");
    let b_host = format!("{}/host/", b.display());
    for line in registry.lines().filter(|l| l.starts_with("data_dir")) {
        assert!(
            line.contains(&b_host),
            "a data_dir still points elsewhere: {line}"
        );
    }
    assert!(
        !registry.contains(&format!("{}/", a.display())),
        "{registry}"
    );
    let locks: Vec<_> = std::fs::read_dir(b.join("golden/runtime"))
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    assert!(locks.is_empty(), "runtime/ keeps no lock: {locks:?}");
    assert!(owner_writable(&b.join("golden")), "the copy is writable");
    let marker_b = marker(&b);
    assert_eq!(marker_b.preset, "release");
    assert_eq!(marker_b.from.as_deref(), Some(a.to_str().unwrap()));
    assert!(marker_b.release_version.is_some());

    let findings = session
        .check(false)
        .await
        .unwrap_or_else(|o| panic!("check: {o:?}"));
    assert_eq!(
        findings,
        Vec::new(),
        "the imported golden passes acceptance"
    );

    // The imported library serves: the pending job drains into gamma.
    session
        .daemon_start(DaemonStart::library("beta"))
        .await
        .unwrap_or_else(|o| panic!("daemon start: {o:?}"));
    for args in [["queue", "resume"], ["queue", "wait"]] {
        let out = session
            .exec(&args.map(Into::into), ExecOpts::default())
            .await
            .unwrap_or_else(|o| panic!("{args:?}: {o:?}"));
        assert_eq!(
            (out.outcome.class, out.outcome.code),
            (Class::Exited, Some(0)),
            "{args:?}: {:?}",
            out.outcome
        );
    }
    session
        .daemon_stop(Duration::from_secs(30))
        .await
        .unwrap_or_else(|o| panic!("daemon stop: {o:?}"));
    session
        .snapshot()
        .await
        .unwrap_or_else(|o| panic!("snapshot: {o:?}"));
    session
        .restore()
        .await
        .unwrap_or_else(|o| panic!("restore: {o:?}"));

    assert_eq!(
        signature(&a.join("golden")),
        a_sig,
        "the frozen golden is untouched"
    );
    assert_eq!(modes(&a.join("golden")), a_modes, "its permissions too");
}

#[tokio::test]
async fn a_tampered_release_golden_is_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let a = root.path().join("a");
    drop(build_rich(&a, &root.path().join("ws-a")).await);
    let a = freeze(&a).await;
    let catalog = a.join("golden/roots/alpha/catalog.db");
    let mut mode = std::fs::metadata(&catalog).unwrap().permissions();
    mode.set_mode(mode.mode() | 0o200);
    std::fs::set_permissions(&catalog, mode).expect("chmod u+w");
    let mut bytes = std::fs::read(&catalog).expect("catalog");
    let at = bytes.len() / 2;
    bytes[at] ^= 0xff;
    std::fs::write(&catalog, bytes).expect("write catalog");

    let b = root.path().join("b");
    let refused = import(&b, &root.path().join("ws-b"), &a, false)
        .await
        .err()
        .expect("a changed golden is refused");
    assert_eq!(refused.stage, STAGE_PRESET, "{refused:?}");
    assert!(
        summary(&refused).contains("has changed since it was frozen"),
        "{refused:?}"
    );
    assert!(!b.exists(), "the target is not created");

    // With --replace over an ordinary sandbox: that sandbox stays whole.
    let c = root.path().join("c");
    drop(build_bare(&c, &root.path().join("ws-c")).await);
    let c = c.canonicalize().unwrap();
    let c_sig = signature(&c);
    let refused = import(&c, &root.path().join("ws-c"), &a, true)
        .await
        .err()
        .expect("a changed golden is refused with --replace too");
    assert_eq!(refused.stage, STAGE_PRESET, "{refused:?}");
    assert_eq!(signature(&c), c_sig, "the target was not cleared");
    assert!(marker(&c).ready);
}

#[tokio::test]
async fn a_target_path_that_needs_escaping_is_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let a = root.path().join("a");
    drop(build_rich(&a, &root.path().join("ws-a")).await);
    let a = freeze(&a).await;

    for target in [root.path().join("q\"uote"), root.path().join("back\\slash")] {
        let refused = import(&target, &root.path().join("ws-b"), &a, false)
            .await
            .err()
            .unwrap_or_else(|| panic!("{} is refused", target.display()));
        assert_eq!(refused.stage, STAGE_PATHS, "{refused:?}");
        assert!(!target.exists(), "{} was created", target.display());
    }

    // An existing sandbox renamed onto such a path, with --replace.
    let plain = root.path().join("plain");
    drop(build_bare(&plain, &root.path().join("ws-c")).await);
    let renamed = root.path().join("ren\"amed");
    std::fs::rename(&plain, &renamed).expect("rename");
    let before = signature(&renamed);
    let refused = import(&renamed, &root.path().join("ws-c"), &a, true)
        .await
        .err()
        .expect("a renamed target is refused");
    assert_eq!(refused.stage, STAGE_PATHS, "{refused:?}");
    assert_eq!(signature(&renamed), before, "the sandbox was not touched");
}

#[tokio::test]
async fn an_old_path_outside_the_rewrite_set_is_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let a = root.path().join("a");
    drop(build_rich(&a, &root.path().join("ws-a")).await);
    let a_canonical = a.canonicalize().unwrap();
    let old_path = format!("{}/host/roots/alpha", a_canonical.display());
    std::fs::write(a.join("golden/notes.txt"), &old_path).expect("notes");
    let a = freeze(&a).await;
    let b = root.path().join("b");
    let refused = import(&b, &root.path().join("ws-b"), &a, false)
        .await
        .err()
        .expect("an old path outside the rewrite set is refused");
    assert_eq!(refused.stage, STAGE_PRESET, "{refused:?}");
    assert!(detail(&refused).contains("notes.txt"), "{refused:?}");

    // The same bytes in a log are history and travel as they are.
    let a2 = root.path().join("a2");
    drop(build_rich(&a2, &root.path().join("ws-a2")).await);
    let a2_canonical = a2.canonicalize().unwrap();
    let old_path = format!("{}/host/roots/alpha", a2_canonical.display());
    let log_dir = a2.join("golden/daemon-state/logs");
    std::fs::create_dir_all(&log_dir).expect("logs");
    std::fs::write(log_dir.join("note.log"), &old_path).expect("note.log");
    let a2 = freeze(&a2).await;
    let b2 = root.path().join("b2");
    let session = import(&b2, &root.path().join("ws-b2"), &a2, false)
        .await
        .unwrap_or_else(|o| panic!("import: {o:?}"));
    let kept = std::fs::read_to_string(
        session
            .paths()
            .dir()
            .join("golden/daemon-state/logs/note.log"),
    )
    .expect("the log travelled");
    assert_eq!(kept, old_path);
}

#[tokio::test]
async fn overlapping_paths_are_refused_before_anything_moves() {
    let root = tempfile::tempdir().expect("tempdir");
    let a = root.path().join("a");
    drop(build_rich(&a, &root.path().join("ws-a")).await);
    let a = freeze(&a).await;
    let a_sig = signature(&a);
    let b = root.path().join("b");
    drop(build_bare(&b, &root.path().join("ws-b")).await);
    let b = b.canonicalize().unwrap();
    let b_sig = signature(&b);

    let cases = [
        ("the source is the target", b.clone()),
        ("the source is inside the target", b.join("inner")),
        ("the target is inside the source", root.path().to_path_buf()),
    ];
    for (case, from) in cases {
        let refused = import(&b, &root.path().join("ws-b"), &from, true)
            .await
            .err()
            .unwrap_or_else(|| panic!("{case}: refused"));
        assert_eq!(refused.stage, STAGE_PATHS, "{case}: {refused:?}");
        assert!(summary(&refused).contains("overlap"), "{case}: {refused:?}");
        assert_eq!(signature(&b), b_sig, "{case}: the target was touched");
        assert_eq!(signature(&a), a_sig, "{case}: the source was touched");
    }

    // A target that does not exist yet, inside the frozen source.
    let nested = a.join("nested").join("b");
    let refused = import(&nested, &root.path().join("ws-b"), &a, false)
        .await
        .err()
        .expect("a target inside the source is refused");
    assert_eq!(refused.stage, STAGE_PATHS, "{refused:?}");
    assert!(summary(&refused).contains("overlap"), "{refused:?}");
    assert!(!a.join("nested").exists());
    assert_eq!(signature(&a), a_sig);
}

#[tokio::test]
async fn a_source_that_changes_during_the_copy_is_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let a = root.path().join("a");
    drop(build_rich(&a, &root.path().join("ws-a")).await);
    let a = freeze(&a).await;
    let b = root.path().join("b");
    let staging = b.join("golden.import");
    let golden_a = a.join("golden");
    let candidates = [
        "registry.toml",
        "daemon-state/queue.json",
        "inputs/manifest.json",
        "index-profiles/sandbox-flat.toml",
    ];
    let mut changed = false;
    let hooks = CreateHooks {
        on_import_copy: Some(Box::new(move |files: usize| {
            if files == 1 && !changed {
                // Change a file the copy has not reached yet.
                for candidate in candidates {
                    if staging.join(candidate).exists() {
                        continue;
                    }
                    let path = golden_a.join(candidate);
                    let mut mode = std::fs::metadata(&path)?.permissions();
                    mode.set_mode(mode.mode() | 0o200);
                    std::fs::set_permissions(&path, mode)?;
                    let mut bytes = std::fs::read(&path)?;
                    bytes.extend_from_slice(b"\n# changed during the copy\n");
                    std::fs::write(&path, bytes)?;
                    changed = true;
                    break;
                }
            }
            Ok(())
        })),
        ..CreateHooks::default()
    };
    let refused = import_with(&b, &root.path().join("ws-b"), &a, false, hooks)
        .await
        .err()
        .expect("a source that changes during the copy is refused");
    assert_eq!(refused.stage, STAGE_PRESET, "{refused:?}");
    assert!(
        summary(&refused).contains("changed while it was copied"),
        "{refused:?}"
    );
    assert!(
        !b.join("golden.import").exists(),
        "the staging tree is removed"
    );
    assert!(!b.join("golden").exists(), "no golden was put in place");
    assert!(!marker(&b).ready, "the target stays not ready");
}
