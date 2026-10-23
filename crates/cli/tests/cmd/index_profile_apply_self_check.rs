// SPDX-License-Identifier: Apache-2.0

//! The self-check at the end of `bookrack index-profile apply` names
//! both dimensions when the declared one is what keeps the library from
//! converging.
//!
//! A library with chunks and no stamps at all plans a stamp
//! reconciliation and nothing else. The reconciliation stamps the
//! dimension the backend produces, which is [`EmbedStub::DIMENSION`];
//! the profile declares the default model at its registry dimension, so
//! the re-derived plan still diverges, and the report has to say by how
//! much rather than only that it does.

#![cfg(unix)]

use std::time::{Duration, Instant};

use bookrack_config::DEFAULT_EMBED_MODEL;
use bookrack_corpus::Corpus;
use bookrack_test_support::{EmbedStub, Sandbox, Spawn, bookrack_cmd};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

const LIBRARY: &str = "main";
const PROFILE: &str = "scan-default-model";

/// The per-user profile directory, beside the registry file.
const USER_PROFILE_DIR: &str = "index-profiles";

/// The registry dimension of [`DEFAULT_EMBED_MODEL`].
const DEFAULT_MODEL_DIM: u32 = 1024;

fn profile_toml() -> String {
    format!(
        "schema_version = 1\n\
         name = \"{PROFILE}\"\n\
         description = \"The default embed model with an exhaustive scan.\"\n\
         \n\
         [embed]\n\
         backend = \"ollama\"\n\
         model = \"{DEFAULT_EMBED_MODEL}\"\n\
         dim = {DEFAULT_MODEL_DIM}\n\
         \n\
         [ann]\n\
         kind = \"brute-force\"\n\
         num_partitions = 1\n\
         num_bits = 8\n\
         nprobes = 1\n\
         refine_factor = 1\n\
         \n\
         [reranker]\n\
         kind = \"none\"\n"
    )
}

const BOOK_TEXT: &str = "\
Chapter One

The synthetic cartographer folded the synthetic chart and began to
count. Every island on the chart had a fictional harbour, and every
fictional harbour had a fictional tide table. Counting the harbours
took the cartographer most of the afternoon.

Chapter Two

On the second day the chart grew a compass rose, and the cartographer
ordered the winds from calm to gale.
";

async fn run(spawn: Spawn, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::from(spawn.build())
        .args(args)
        .output()
        .await
        .expect("run the bookrack client");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn a_dimension_left_divergent_is_reported_with_both_values() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root(LIBRARY);
    let registry = sandbox.write_registry_entries(Some(LIBRARY), &[(LIBRARY, root.as_path())]);
    let profiles = registry
        .parent()
        .expect("the registry has a parent")
        .join(USER_PROFILE_DIR);
    std::fs::create_dir_all(&profiles).expect("create the user profile directory");
    std::fs::write(profiles.join(format!("{PROFILE}.toml")), profile_toml())
        .expect("write the user profile");

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::url())
            .build(),
    );
    daemon_cmd.args(["--library", LIBRARY, "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&sandbox.tty_lock_path(), Duration::from_secs(30)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    let book = sandbox.path().join("synthetic-chart.txt");
    std::fs::write(&book, BOOK_TEXT).expect("write the synthetic book");
    let book_arg = book.to_str().expect("utf-8 path");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (code, _, stderr) = run(
            bookrack_cmd!(&sandbox).without_data_dir(),
            &["ingest", book_arg],
        )
        .await;
        if code == Some(0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the fixture ingest never succeeded: {stderr}",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A library with chunks, no stamps, and no persisted ANN state: the
    // only action the plan derives is the stamp reconciliation.
    Corpus::open(&root.join("corpus.db"))
        .expect("open the books corpus")
        .clear_index_stamps()
        .expect("clear the books stamps");
    let meta = root.join("lancedb").join("vectors_meta.json");
    if meta.exists() {
        std::fs::remove_file(&meta).expect("remove the persisted ANN state");
    }

    let (code, stdout, stderr) = run(
        bookrack_cmd!(&sandbox).without_data_dir(),
        &[
            "--library",
            LIBRARY,
            "index-profile",
            "apply",
            PROFILE,
            "--dry-run",
        ],
    )
    .await;
    assert_eq!(code, Some(0), "the dry run plans offline: {stderr}");
    assert!(stdout.contains("stamps reconcile"), "{stdout}");
    assert!(!stdout.contains("vectors reset"), "{stdout}");
    assert!(!stdout.contains("vectors drop"), "{stdout}");

    let (code, stdout, stderr) = run(
        bookrack_cmd!(&sandbox).without_data_dir(),
        &[
            "--library",
            LIBRARY,
            "index-profile",
            "apply",
            PROFILE,
            "--yes",
        ],
    )
    .await;
    assert_ne!(code, Some(0), "the library cannot converge: {stdout}");
    assert!(stderr.contains("still diverges"), "{stderr}");
    for needle in [
        format!("stamped {} dimensions", EmbedStub::DIMENSION),
        format!("declares {DEFAULT_MODEL_DIM}"),
    ] {
        assert!(
            stderr.contains(&needle),
            "the self-check must name both dimensions; {needle:?} missing: {stderr}",
        );
    }

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(10)).await;
}
