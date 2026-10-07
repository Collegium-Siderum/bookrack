// SPDX-License-Identifier: Apache-2.0

//! `bookrack verify` judges the report it prints. A finding that says a
//! store is damaged, or that an intake row names a file that is gone,
//! exits 1; a library whose stores read back and whose files are in
//! place exits 0.
//!
//! Two libraries on one daemon make the test discriminate: `whole`
//! carries both stores and an intake whose file is on disk, `torn` the
//! same intake row with the file absent. Give both libraries the same
//! file state and the two exit codes agree whatever the verdict does.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::ItemKind;
use bookrack_corpus::Corpus;
use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

const STORED_PATH: &str = "aa/bb/source.txt";

/// A library with both stores and one intake row whose `stored_path`
/// is `STORED_PATH`; the file itself is written only when
/// `file_present`.
fn library_with_one_intake(root: &Path, file_present: bool) {
    let mut catalog = Catalog::open(&root.join("catalog.db")).expect("open the catalog");
    let intake_id = catalog
        .register_intake(ItemKind::Book, &NewIntake::new("sha-1"))
        .expect("register an intake")
        .into_intake()
        .intake_id;
    catalog
        .set_stored_path(ItemKind::Book, intake_id, STORED_PATH)
        .expect("record the stored path");
    drop(catalog);
    drop(Corpus::open(&root.join("corpus.db")).expect("open the corpus"));
    if file_present {
        let file = root.join("books").join(STORED_PATH);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("books dir");
        std::fs::write(&file, b"source bytes").expect("write the stored file");
    }
}

#[tokio::test]
async fn verify_exits_one_on_a_missing_intake_file_and_zero_on_a_whole_library() {
    let sandbox = Sandbox::new();
    let whole = sandbox.data_root("whole-root");
    let torn = sandbox.data_root("torn-root");
    library_with_one_intake(&whole, true);
    library_with_one_intake(&torn, false);
    sandbox.write_registry_entries(
        Some("whole"),
        &[("whole", whole.as_path()), ("torn", torn.as_path())],
    );

    let lock_path = sandbox.tty_lock_path();
    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::url())
            .build(),
    );
    daemon_cmd.args(["--library", "torn", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&lock_path, Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    let verify = |args: &'static [&'static str]| {
        let sandbox = &sandbox;
        async move {
            let output = Command::from(bookrack_cmd!(sandbox).without_data_dir().build())
                .args(args)
                .output()
                .await
                .expect("run bookrack verify");
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            (output.status.code(), stdout, stderr)
        }
    };

    // The control: a library whose stores read back and whose one file
    // is on disk.
    let (code, stdout, stderr) = verify(&["--library", "whole", "verify"]).await;
    assert_eq!(
        code,
        Some(0),
        "a whole library must exit 0: stdout={stdout} stderr={stderr}"
    );

    // The finding: the same row, file gone.
    let (code, stdout, stderr) = verify(&["--library", "torn", "verify"]).await;
    assert_eq!(
        code,
        Some(1),
        "a missing intake file must exit 1: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("intake 1"),
        "the report must name the intake whose file is missing: {stdout}"
    );
    assert!(
        !stderr.lines().any(|l| l.starts_with("bookrack:")),
        "the report is the failure surface; the reporter must add no line: {stderr}"
    );

    // `--json` keeps the wire shape on stdout and the verdict in the
    // exit code, so a script can have both.
    let (code, stdout, _) = verify(&["--json", "--library", "torn", "verify"]).await;
    assert_eq!(
        code,
        Some(1),
        "the verdict does not depend on the output mode"
    );
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--json output is not JSON ({e}): {stdout}"));
    assert_eq!(
        report["missing_intake_files"],
        serde_json::json!([1]),
        "{report}"
    );

    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(5)).await;
}
