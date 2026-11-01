// SPDX-License-Identifier: Apache-2.0

//! The `rich` preset against the real `bookrack` binary: it builds, its
//! shape check passes on what it built, and the check finds exactly the
//! damage done to a copy of it, one kind at a time. The check also stops
//! its daemon and restores the tree when a read fails part way.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_core::Problem;
use bookrack_test_support::bookrack_bin;
use bookrack_test_support::tool::exec::{DaemonStart, ExecOpts};
use bookrack_test_support::tool::rich::ShapeFinding;
use bookrack_test_support::tool::snapshot::copy_tree;
use bookrack_test_support::tool::{
    Class, CreateOpts, Outcome, Preset, STAGE_TOOL, Session, SessionOpts, Source, sig,
};
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
    std::fs::create_dir(root.path().join("ws")).expect("ws");
    let mut session = Session::create(
        &root.path().join("sbx"),
        Source::Prebuilt(bookrack_bin!()),
        Preset::Rich,
        CreateOpts {
            replace: false,
            workspace: Some(root.path().join("ws")),
        },
        SessionOpts::default(),
    )
    .await
    .unwrap_or_else(|o| panic!("create: {o:?}"));
    let dir = session.paths().dir().to_path_buf();
    let golden = dir.join("golden");
    assert_eq!(check(&mut session).await, Vec::new(), "the preset as built");

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
