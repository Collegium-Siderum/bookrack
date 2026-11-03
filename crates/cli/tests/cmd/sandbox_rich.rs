// SPDX-License-Identifier: Apache-2.0

//! The `rich` preset against the real `bookrack` binary: it builds, its
//! shape check and its acceptance check pass on what it built, and each
//! check finds exactly the damage done to a copy of it, one kind at a
//! time. Both checks stop their daemon and restore the tree when a read
//! fails part way, and the acceptance check refuses a stub it cannot
//! read through.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_core::Problem;
use bookrack_test_support::tool::exec::{DaemonStart, ExecOpts};
use bookrack_test_support::tool::rich::ShapeFinding;
use bookrack_test_support::tool::snapshot::copy_tree;
use bookrack_test_support::tool::{
    Class, CreateHooks, CreateOpts, Outcome, Preset, Record, STAGE_PRESET, STAGE_STUB, STAGE_TOOL,
    Session, SessionOpts, Source, sig,
};
use bookrack_test_support::{StubSpec, Vectors, bookrack_bin};
use serde_json::Value;

fn keys(findings: &[ShapeFinding]) -> BTreeSet<String> {
    findings.iter().map(|f| f.key.clone()).collect()
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

/// Put the pristine copy back as `golden/`.
fn reset_golden(golden: &Path, pristine: &Path) {
    std::fs::remove_dir_all(golden).expect("remove golden");
    copy_tree(pristine, golden, &mut |_| Ok(())).expect("copy the pristine golden back");
}

/// Drop one `[libraries.<name>]` table from a registry.
fn drop_library(registry: &Path, name: &str) {
    let text = std::fs::read_to_string(registry).expect("registry");
    let header = format!("[libraries.{name}]");
    let kept: Vec<&str> = text
        .split("\n\n")
        .filter(|block| !block.trim_start().starts_with(&header))
        .collect();
    assert_ne!(
        kept.join("\n\n"),
        text,
        "premise: the registry names {name}"
    );
    std::fs::write(registry, kept.join("\n\n")).expect("write registry");
}

async fn check(session: &mut Session) -> Vec<ShapeFinding> {
    session
        .check_shape()
        .await
        .unwrap_or_else(|o| panic!("check: {o:?}"))
}

async fn accept(session: &mut Session, shape: bool) -> Vec<ShapeFinding> {
    session
        .check(shape)
        .await
        .unwrap_or_else(|o| panic!("check: {o:?}"))
}

/// A rich sandbox built under `root`, from a workspace directory beside
/// it.
async fn build_rich(root: &Path, hooks: CreateHooks) -> Result<Session, Outcome> {
    std::fs::create_dir_all(root.join("ws")).expect("ws");
    Session::create_with(
        &root.join("sbx"),
        Source::Prebuilt(bookrack_bin!()),
        Preset::Rich,
        CreateOpts {
            replace: false,
            workspace: Some(root.join("ws")),
        },
        SessionOpts::default(),
        hooks,
    )
    .await
}

/// The records under `.calls/`, oldest first.
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

/// The `<key>: <detail>` lines of a problem's detail, as keys.
fn detail_keys(outcome: &Outcome) -> BTreeSet<String> {
    outcome
        .problem
        .as_ref()
        .and_then(|p| p.data.detail.clone())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(": ").map(|(k, _)| k.to_string()))
        .collect()
}

async fn stdout_json(session: &mut Session, args: &[&str]) -> Value {
    let args: Vec<_> = args.iter().map(|a| a.into()).collect();
    let out = session
        .exec(&args, ExecOpts::default())
        .await
        .unwrap_or_else(|o| panic!("exec: {o:?}"));
    assert_eq!(
        (out.outcome.class, out.outcome.code),
        (Class::Exited, Some(0)),
        "{:?}",
        out.outcome
    );
    match out.output {
        bookrack_test_support::Output::Memory { stdout, .. } => {
            serde_json::from_slice(&stdout).expect("stdout is JSON")
        }
        other => panic!("a memory capture came back as {other:?}"),
    }
}

fn unfinished_daemons(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir.join("run/daemon"))
        .expect("run/daemon")
        .map(|e| e.expect("entry").path())
        .filter(|p| !p.join("exit.json").exists() && !p.join("recovered.json").exists())
        .collect()
}

#[tokio::test]
async fn the_rich_preset_builds_and_its_check_finds_each_kind_of_damage() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut session = build_rich(root.path(), CreateHooks::default())
        .await
        .unwrap_or_else(|o| panic!("create: {o:?}"));
    let dir = session.paths().dir().to_path_buf();
    let golden = dir.join("golden");
    let created = records(&dir)
        .into_iter()
        .find(|r| r.command == "new")
        .expect("the new record");
    assert!(
        created.notes.iter().any(|n| n == "acceptance: 0 findings"),
        "the build records its acceptance: {:?}",
        created.notes
    );
    assert_eq!(check(&mut session).await, Vec::new(), "the preset as built");

    // The acceptance check, with and without the shape check, on the
    // session that still holds the build's stub.
    let both = accept(&mut session, true).await;
    assert_eq!(both, Vec::new(), "the preset as built, both checks");
    assert!(
        !both.iter().any(|f| f.key.ends_with(".parse")),
        "every reading parsed: {both:?}"
    );
    // A fresh session has no stub; the check starts one of its own.
    let mut reopened =
        Session::open(&dir, SessionOpts::default()).unwrap_or_else(|o| panic!("open: {o:?}"));
    assert!(reopened.stub_url().is_none());
    assert_eq!(accept(&mut reopened, false).await, Vec::new());
    assert!(
        reopened.stub_url().is_some(),
        "the check leaves the stub it started in the session"
    );
    drop(reopened);

    let pristine = root.path().join("pristine");
    copy_tree(&golden, &pristine, &mut |_| Ok(())).expect("keep a pristine golden");

    // 1. A library missing from the registry.
    reset_golden(&golden, &pristine);
    drop_library(&golden.join("registry.toml"), "delta");
    assert_eq!(
        keys(&check(&mut session).await),
        set(&["libraries.count", "status.unmounted"]),
        "delta dropped from the registry"
    );

    // 2. The failed job's path renamed; the queue stays paused, so the
    //    worker leaves the pending job alone.
    reset_golden(&golden, &pristine);
    let queue = golden.join("daemon-state/queue.json");
    let mut doc: Value =
        serde_json::from_slice(&std::fs::read(&queue).expect("queue.json")).expect("queue JSON");
    let failed = doc["jobs"]
        .as_array_mut()
        .expect("jobs")
        .iter_mut()
        .find(|j| j["state"] == "failed")
        .expect("a failed job");
    failed["path"] = Value::from(
        failed["path"]
            .as_str()
            .expect("a path")
            .replace("broken.epub", "renamed.epub"),
    );
    std::fs::write(&queue, serde_json::to_vec_pretty(&doc).expect("encode")).expect("write");
    assert_eq!(
        keys(&check(&mut session).await),
        set(&["queue.failed_path"]),
        "the failed job renamed"
    );

    // 3. A book removed from beta through the daemon, then snapshotted.
    reset_golden(&golden, &pristine);
    session
        .restore()
        .await
        .unwrap_or_else(|o| panic!("restore: {o:?}"));
    session
        .daemon_start(DaemonStart::library("beta"))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    let list = stdout_json(&mut session, &["--library", "beta", "list", "--json"]).await;
    let id = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["source_filename"] == "b3.txt")
        .and_then(|row| row["intake_id"].as_u64())
        .expect("b3.txt is listed in beta");
    let removed = session
        .exec(
            &[
                "--library".into(),
                "beta".into(),
                "remove".into(),
                id.to_string().into(),
                "--yes".into(),
            ],
            ExecOpts::default(),
        )
        .await
        .unwrap_or_else(|o| panic!("remove: {o:?}"));
    assert_eq!(
        (removed.outcome.class, removed.outcome.code),
        (Class::Exited, Some(0)),
        "{:?}",
        removed.outcome
    );
    session
        .daemon_stop(Duration::from_secs(30))
        .await
        .unwrap_or_else(|o| panic!("stop: {o:?}"));
    session
        .snapshot()
        .await
        .unwrap_or_else(|o| panic!("snapshot: {o:?}"));
    assert_eq!(
        keys(&check(&mut session).await),
        set(&["list.beta.books"]),
        "a book removed from beta"
    );

    // A read that does not exit 0 is reported as such, not parsed.
    reset_golden(&golden, &pristine);
    std::fs::remove_dir_all(golden.join("roots/gamma")).expect("remove gamma's root");
    let found = keys(&check(&mut session).await);
    assert!(found.contains("list.gamma.exec"), "{found:?}");
    assert!(!found.contains("list.gamma.parse"), "{found:?}");

    // A read that fails part way still stops the daemon and restores.
    reset_golden(&golden, &pristine);
    let mut reads = 0;
    let failed = session
        .check_shape_with(&mut |_| {
            reads += 1;
            if reads == 3 {
                Err(Outcome::tool_error(
                    STAGE_TOOL,
                    Problem::new("injected read failure"),
                ))
            } else {
                Ok(())
            }
        })
        .await
        .expect_err("the injected failure comes back");
    assert_eq!(
        failed.problem.as_ref().map(|p| p.summary.as_str()),
        Some("injected read failure"),
        "{failed:?}"
    );
    assert_eq!(unfinished_daemons(&dir), Vec::<PathBuf>::new());
    assert_eq!(
        sig::signature(&dir.join("host")).expect("sign host"),
        sig::signature(&golden).expect("sign golden"),
        "the host was not restored after the failed read"
    );
}

/// The acceptance check reads through a 1024-wide topic stub and
/// nothing else: a detached stub in another fill is refused before a
/// daemon starts, and so is one this process started at another width.
#[tokio::test]
async fn a_check_refuses_a_stub_that_is_not_topic() {
    let root = tempfile::tempdir().expect("tempdir");
    let session = build_rich(root.path(), CreateHooks::default())
        .await
        .unwrap_or_else(|o| panic!("create: {o:?}"));
    let dir = session.paths().dir().to_path_buf();
    drop(session);

    // A detached stub is known by its start record; the check reads
    // that record rather than asking the stub, so a record is enough
    // to stand for one.
    let instance = dir.join("run/stub/1");
    std::fs::create_dir_all(&instance).expect("stub instance");
    std::fs::write(
        instance.join("start.json"),
        serde_json::json!({
            "dimension": 1024, "vectors": "spread", "identity": "forged-1",
            "started": 0.0, "parent_pid": std::process::id(),
        })
        .to_string(),
    )
    .expect("start.json");
    let mut detached =
        Session::open(&dir, SessionOpts::default()).unwrap_or_else(|o| panic!("open: {o:?}"));
    let daemons_before = std::fs::read_dir(dir.join("run/daemon"))
        .expect("run/daemon")
        .count();
    let refused = detached
        .check(false)
        .await
        .expect_err("a spread stub is refused");
    assert_eq!(refused.stage, STAGE_STUB, "{refused:?}");
    assert_eq!(
        std::fs::read_dir(dir.join("run/daemon"))
            .expect("run/daemon")
            .count(),
        daemons_before,
        "no daemon was started"
    );
    drop(detached);
    std::fs::remove_dir_all(&instance).expect("remove the stub instance");

    let mut narrow =
        Session::open(&dir, SessionOpts::default()).unwrap_or_else(|o| panic!("open: {o:?}"));
    narrow.stub_start(StubSpec::new(8, Vectors::Topic, None).expect("a valid spec"));
    let refused = narrow
        .check(false)
        .await
        .expect_err("an 8-wide stub is refused");
    assert_eq!(refused.stage, STAGE_STUB, "{refused:?}");
}

/// A book removed from a served library after the build fails the
/// acceptance check under every key that read it, and the call is
/// recorded as refused.
#[tokio::test]
async fn a_golden_missing_a_book_fails_acceptance() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut session = build_rich(root.path(), CreateHooks::default())
        .await
        .unwrap_or_else(|o| panic!("create: {o:?}"));
    let dir = session.paths().dir().to_path_buf();
    session
        .restore()
        .await
        .unwrap_or_else(|o| panic!("restore: {o:?}"));
    session
        .daemon_start(DaemonStart::library("beta"))
        .await
        .unwrap_or_else(|o| panic!("start: {o:?}"));
    let list = stdout_json(&mut session, &["--library", "beta", "list", "--json"]).await;
    let id = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["source_filename"] == "b3.txt")
        .and_then(|row| row["intake_id"].as_u64())
        .expect("b3.txt is listed in beta");
    let removed = session
        .exec(
            &[
                "--library".into(),
                "beta".into(),
                "remove".into(),
                id.to_string().into(),
                "--yes".into(),
            ],
            ExecOpts::default(),
        )
        .await
        .unwrap_or_else(|o| panic!("remove: {o:?}"));
    assert_eq!(
        (removed.outcome.class, removed.outcome.code),
        (Class::Exited, Some(0)),
        "{:?}",
        removed.outcome
    );
    session
        .daemon_stop(Duration::from_secs(30))
        .await
        .unwrap_or_else(|o| panic!("stop: {o:?}"));
    session
        .snapshot()
        .await
        .unwrap_or_else(|o| panic!("snapshot: {o:?}"));

    assert_eq!(
        keys(&accept(&mut session, false).await),
        set(&[
            "books.beta",
            "chunks.beta",
            "source.b3",
            "hit.b3.c1",
            "hit.b3.c2",
            "hit.b3.c3",
        ]),
        "a book removed from beta"
    );
    let last = records(&dir).pop().expect("the check's record");
    assert_eq!(last.command, "check");
    assert_eq!(last.class, Class::Refused, "{last:?}");
    assert_eq!(last.stage, STAGE_PRESET, "{last:?}");
}

/// Inputs that keep only each chapter's first paragraph still build,
/// pass the shape check, and fail the acceptance check on the chapters
/// and the source hash: the two checks are independent.
#[tokio::test]
async fn a_golden_that_kept_only_the_unique_sentences_fails_acceptance() {
    let root = tempfile::tempdir().expect("tempdir");
    let hooks = CreateHooks {
        on_inputs: Some(Box::new(|inputs: &Path| {
            let path = inputs.join("b1.txt");
            let text = std::fs::read_to_string(&path)?;
            let kept: Vec<&str> = text
                .split("\n\n")
                .filter(|p| !p.starts_with("Paragraph "))
                .collect();
            std::fs::write(&path, format!("{}\n\n", kept.join("\n\n")))
        })),
        ..CreateHooks::default()
    };
    let failed = build_rich(root.path(), hooks)
        .await
        .err()
        .expect("the build fails its acceptance check");
    assert_eq!(failed.stage, STAGE_PRESET, "{failed:?}");
    let summary = failed
        .problem
        .as_ref()
        .map(|p| p.summary.as_str())
        .unwrap_or_default();
    assert!(
        summary.contains("does not match its inputs"),
        "the acceptance check, not the shape check, failed the build: {summary}"
    );
    assert_eq!(
        detail_keys(&failed),
        set(&["hit.b1.c1", "hit.b1.c2", "hit.b1.c3", "source.b1"]),
        "{failed:?}"
    );
}

/// Whatever fails part way — a read, a command, the stub — the check
/// leaves no daemon behind, the host restored, and `golden/` as it was.
#[tokio::test]
async fn acceptance_cleans_up_on_every_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut session = build_rich(root.path(), CreateHooks::default())
        .await
        .unwrap_or_else(|o| panic!("create: {o:?}"));
    let dir = session.paths().dir().to_path_buf();
    let golden = dir.join("golden");
    let pristine = root.path().join("pristine");
    copy_tree(&golden, &pristine, &mut |_| Ok(())).expect("keep a pristine golden");
    let golden_before = sig::signature(&golden).expect("sign golden");
    let injected = |name: &str| {
        Outcome::tool_error(
            STAGE_TOOL,
            Problem::new(format!("injected failure at {name}")),
        )
    };

    // A read refused part way.
    let mut reads = 0;
    let failed = session
        .check_with(false, &mut |name| {
            reads += 1;
            if reads == 3 {
                Err(injected(name))
            } else {
                Ok(())
            }
        })
        .await
        .expect_err("the injected failure comes back");
    assert!(
        failed
            .problem
            .as_ref()
            .is_some_and(|p| p.summary.starts_with("injected failure")),
        "{failed:?}"
    );
    assert_clean(&dir, &golden, &golden_before);

    // A read whose command fails: gamma's root gone. The tree is
    // restored to the damaged golden, which is what the check was asked
    // about; the damage is then undone for the cases after it.
    std::fs::remove_dir_all(golden.join("roots/gamma")).expect("remove gamma's root");
    let damaged = sig::signature(&golden).expect("sign golden");
    let found = keys(&accept(&mut session, false).await);
    assert!(found.contains("books.gamma.exec"), "{found:?}");
    assert_clean(&dir, &golden, &damaged);
    reset_golden(&golden, &pristine);
    session
        .restore()
        .await
        .unwrap_or_else(|o| panic!("restore: {o:?}"));
    assert_clean(&dir, &golden, &golden_before);

    // A read refused just before the daemon would be stopped.
    let failed = session
        .check_with(false, &mut |name| {
            if name == "reference.Coldwater, Sela" {
                Err(injected(name))
            } else {
                Ok(())
            }
        })
        .await
        .expect_err("the injected failure comes back");
    assert!(
        failed
            .problem
            .as_ref()
            .is_some_and(|p| p.summary.ends_with("reference.Coldwater, Sela")),
        "{failed:?}"
    );
    assert_clean(&dir, &golden, &golden_before);
    drop(session);

    // The stub refused before any daemon starts.
    let mut narrow =
        Session::open(&dir, SessionOpts::default()).unwrap_or_else(|o| panic!("open: {o:?}"));
    narrow.stub_start(StubSpec::new(8, Vectors::Topic, None).expect("a valid spec"));
    let refused = narrow
        .check(false)
        .await
        .expect_err("an 8-wide stub is refused");
    assert_eq!(refused.stage, STAGE_STUB, "{refused:?}");
    assert_clean(&dir, &golden, &golden_before);
}

fn assert_clean(dir: &Path, golden: &Path, golden_before: &[String]) {
    assert_eq!(unfinished_daemons(dir), Vec::<PathBuf>::new());
    let host = sig::signature(&dir.join("host")).expect("sign host");
    let golden_now = sig::signature(golden).expect("sign golden");
    let drift = sig::diff(&golden_now, &host);
    assert!(
        drift.is_empty(),
        "the host was not restored:\n{}",
        sig::compact_diff(&drift)
    );
    let drift = sig::diff(golden_before, &golden_now);
    assert!(
        drift.is_empty(),
        "golden/ changed:\n{}",
        sig::compact_diff(&drift)
    );
}
