// SPDX-License-Identifier: Apache-2.0

//! `bookrack verify` judges the report it prints. A finding that says a
//! store is damaged, or that an intake row names a file that is gone,
//! exits 1; a library whose stores read back and whose files are in
//! place exits 0.
//!
//! Several libraries on one daemon make each test discriminate: the
//! libraries differ in exactly one fact — a file present or gone, a
//! store at the version this build reads or not — so two exit codes
//! that agree mean the verdict ignored that fact.
//!
//! The embedder probe on the daemon's startup path is answered by
//! [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::ItemKind;
use bookrack_corpus::Corpus;
use bookrack_refs::Refs;
use bookrack_test_support::{EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

const STORED_PATH: &str = "aa/bb/source.txt";

/// Both book stores and one intake row whose `stored_path` is
/// `STORED_PATH`; the file itself is written only when `file_present`.
fn books_with_one_intake(root: &Path, file_present: bool) {
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

/// Both paper stores and one paper intake whose `stored_path` is an
/// absolute path under `papers/`, the shape glean records; the file
/// itself is written only when `file_present`.
fn papers_with_one_intake(root: &Path, file_present: bool) {
    let mut catalog =
        Catalog::open(&root.join("papers_catalog.db")).expect("open the papers catalog");
    let intake_id = catalog
        .register_intake(ItemKind::Paper, &NewIntake::new("sha-paper"))
        .expect("register a paper")
        .into_intake()
        .intake_id;
    let envelope = root.join("papers").join("paper-envelope.json");
    catalog
        .set_stored_path(
            ItemKind::Paper,
            intake_id,
            envelope.to_string_lossy().as_ref(),
        )
        .expect("record the stored path");
    drop(catalog);
    drop(Corpus::open(&root.join("papers_corpus.db")).expect("open the papers corpus"));
    if file_present {
        std::fs::create_dir_all(envelope.parent().expect("parent")).expect("papers dir");
        std::fs::write(&envelope, b"{}").expect("write the envelope");
    }
}

/// Spawn `bookrack run` serving `primary` and wait for its lock.
async fn spawn_daemon(sandbox: &Sandbox, primary: &str) -> DaemonProcess {
    let mut daemon_cmd = Command::from(
        bookrack_cmd!(sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.args(["--library", primary, "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&sandbox.tty_lock_path(), Duration::from_secs(20)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );
    daemon
}

async fn stop_daemon(daemon: DaemonProcess) {
    if let Some(id) = daemon.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(id.to_string())
            .status()
            .await;
    }
    let _ = daemon.wait_with_output(Duration::from_secs(5)).await;
}

/// Run one `bookrack` invocation; returns the exit code, stdout, and
/// stderr.
async fn run_verify(sandbox: &Sandbox, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::from(bookrack_cmd!(sandbox).without_data_dir().build())
        .args(args)
        .output()
        .await
        .expect("run bookrack verify");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.code(), stdout, stderr)
}

/// The `--json` report for `library`, with its exit code.
async fn json_report(sandbox: &Sandbox, library: &str) -> (Option<i32>, Value) {
    let (code, stdout, _) = run_verify(sandbox, &["--json", "--library", library, "verify"]).await;
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--json output is not JSON ({e}): {stdout}"));
    (code, report)
}

#[tokio::test]
async fn verify_exits_one_on_a_missing_intake_file_and_zero_on_a_whole_library() {
    let sandbox = Sandbox::new();
    let whole = sandbox.data_root("whole-root");
    let torn = sandbox.data_root("torn-root");
    let torn_papers = sandbox.data_root("torn-papers-root");
    books_with_one_intake(&whole, true);
    papers_with_one_intake(&whole, true);
    books_with_one_intake(&torn, false);
    papers_with_one_intake(&torn, true);
    books_with_one_intake(&torn_papers, true);
    papers_with_one_intake(&torn_papers, false);
    sandbox.write_registry_entries(
        Some("whole"),
        &[
            ("whole", whole.as_path()),
            ("torn", torn.as_path()),
            ("torn-papers", torn_papers.as_path()),
        ],
    );
    let daemon = spawn_daemon(&sandbox, "torn").await;

    // The control: a library whose stores read back and whose files
    // are on disk, on both sides.
    let (code, stdout, stderr) = run_verify(&sandbox, &["--library", "whole", "verify"]).await;
    assert_eq!(
        code,
        Some(0),
        "a whole library must exit 0: stdout={stdout} stderr={stderr}"
    );

    // The finding: the same rows, the book's file gone.
    let (code, stdout, stderr) = run_verify(&sandbox, &["--library", "torn", "verify"]).await;
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
    let (code, report) = json_report(&sandbox, "torn").await;
    assert_eq!(
        code,
        Some(1),
        "the verdict does not depend on the output mode"
    );
    assert_eq!(
        report["missing_intake_files"],
        serde_json::json!([1]),
        "{report}"
    );

    // The paper side: the same rows, the paper's file gone.
    let (code, stdout, stderr) =
        run_verify(&sandbox, &["--library", "torn-papers", "verify"]).await;
    assert_eq!(
        code,
        Some(1),
        "a missing paper file must exit 1: stdout={stdout} stderr={stderr}"
    );
    let papers_at = stdout
        .find("papers_catalog.db:")
        .unwrap_or_else(|| panic!("the report has no paper section: {stdout}"));
    let finding_at = stdout
        .find("1 missing under papers/:")
        .unwrap_or_else(|| panic!("the report does not name the missing paper: {stdout}"));
    assert!(
        finding_at > papers_at,
        "the missing paper is reported outside the paper section: {stdout}"
    );
    assert!(
        stdout[..papers_at].contains("every stored_path is present on disk"),
        "the book side of the same library was reported as damaged: {stdout}"
    );
    let (code, report) = json_report(&sandbox, "torn-papers").await;
    assert_eq!(code, Some(1), "{report}");
    assert_eq!(
        report["papers"]["missing_intake_files"],
        serde_json::json!([1]),
        "{report}"
    );
    assert_eq!(
        report["missing_intake_files"],
        serde_json::json!([]),
        "{report}"
    );

    stop_daemon(daemon).await;
}

#[tokio::test]
async fn verify_judges_a_root_holding_only_a_reference_store() {
    let sandbox = Sandbox::new();
    let readable = sandbox.data_root("reference-root");
    let behind = sandbox.data_root("reference-behind-root");
    let stores = |root: &PathBuf| {
        drop(Refs::open(&root.join("reference.db")).expect("create reference.db"));
    };
    stores(&readable);
    stores(&behind);
    let conn =
        bookrack_dbkit::open_production(&behind.join("reference.db")).expect("open reference.db");
    conn.pragma_update(None, "user_version", 0)
        .expect("stamp an older version");
    drop(conn);
    sandbox.write_registry_entries(
        Some("reference"),
        &[
            ("reference", readable.as_path()),
            ("reference-behind", behind.as_path()),
        ],
    );
    let daemon = spawn_daemon(&sandbox, "reference").await;

    // The control: the store this build reads, and nothing else.
    let (code, stdout, stderr) = run_verify(&sandbox, &["--library", "reference", "verify"]).await;
    assert_eq!(
        code,
        Some(0),
        "a root holding a readable reference.db must exit 0: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("books:\n  not in use"),
        "the book side of a reference-only root is not reported as unused: {stdout}"
    );
    assert!(
        !stdout.contains("is present)"),
        "the report claims a store that is not on disk: {stdout}"
    );
    let (code, report) = json_report(&sandbox, "reference").await;
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["not_initialised"], false, "{report}");
    assert_eq!(report["reference_schema_ok"], true, "{report}");

    // The finding: the same store at a version the read path refuses.
    let (code, stdout, stderr) =
        run_verify(&sandbox, &["--library", "reference-behind", "verify"]).await;
    assert_eq!(
        code,
        Some(1),
        "an unreadable reference.db must exit 1: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("found user_version 0"),
        "the report does not carry the refusal: {stdout}"
    );

    stop_daemon(daemon).await;
}
