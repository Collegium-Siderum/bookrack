// SPDX-License-Identifier: Apache-2.0

//! `doctor::gather` assembled end to end, against a sandboxed host.
//!
//! The per-section unit tests pin what each row says; nothing below
//! them pinned that `gather` still calls every section, in the order
//! the renderer relies on, or that assembling the report leaves the
//! data root as it found it. Both network probes are answered inside
//! the sandbox: the embedder by `bookrack_test_support::EmbedStub`,
//! the MCP address by a kernel-assigned port nobody serves.

use std::path::Path;

use bookrack_config::LibrarySelection;
use bookrack_runtime::doctor::{Report, Row, Status, gather};
use bookrack_test_support::{ProcessEnv, process_env};

/// Every row label, in report order, for a data root nothing has been
/// ingested into and a registry with no entries. The registry and
/// index-profile sections contribute rows only for registered
/// libraries, and the reranker section only for a profile that
/// enables one; the built-in default does not.
const BARE_ROOT_LABELS: [&str; 19] = [
    "report by",
    "data root",
    "PDFium library",
    "fd limit",
    "catalog.db",
    "corpus.db",
    "lancedb/",
    "papers_catalog.db",
    "papers_corpus.db",
    "lancedb_papers/",
    "reference.db",
    "backup dir",
    "pipeline runs",
    "disk free",
    "daemon state",
    "queue snapshot",
    "Ollama daemon",
    "embed model",
    "MCP endpoint",
];

/// The rows whose status is decided by the data root alone, so a bare
/// root must not fail any of them whatever the host looks like.
const STORE_LABELS: [&str; 10] = [
    "catalog.db",
    "corpus.db",
    "lancedb/",
    "papers_catalog.db",
    "papers_corpus.db",
    "lancedb_papers/",
    "reference.db",
    "backup dir",
    "pipeline runs",
    "queue snapshot",
];

fn selection(data_dir: &Path) -> LibrarySelection {
    LibrarySelection {
        data_dir: Some(data_dir.to_path_buf()),
        library: None,
    }
}

fn labels(report: &Report) -> Vec<&str> {
    report.rows.iter().map(|r| r.label.as_str()).collect()
}

fn row<'a>(report: &'a Report, label: &str) -> &'a Row {
    report
        .rows
        .iter()
        .find(|r| r.label == label)
        .unwrap_or_else(|| panic!("no `{label}` row in the report: {report:?}"))
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the data root")
        .map(|e| e.expect("dir entry").file_name().display().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn gather_assembles_every_section_in_report_order() {
    process_env(ProcessEnv::daemon());
    let root = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempfile::tempdir().expect("tempdir");

    let report = gather(&selection(root.path()), Some(runtime_dir.path())).await;

    assert_eq!(labels(&report), BARE_ROOT_LABELS, "{report:?}");
}

#[tokio::test]
async fn gather_on_a_bare_root_materialises_nothing_and_fails_no_store_row() {
    let sandbox = process_env(ProcessEnv::daemon());
    let root = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempfile::tempdir().expect("tempdir");

    let report = gather(&selection(root.path()), Some(runtime_dir.path())).await;

    // Every store was looked for; none was brought into being, and
    // the daemon's queue document was not written either.
    assert_eq!(entries(root.path()), Vec::<String>::new());
    assert!(
        !sandbox.daemon_state_dir().join("queue.json").exists(),
        "reading the queue snapshot must not create it"
    );
    for label in STORE_LABELS {
        let r = row(&report, label);
        assert!(
            !matches!(r.status, Status::Fail { .. }),
            "a bare root is uninitialised, not broken: {r:?}"
        );
    }
    assert_eq!(row(&report, "catalog.db").value, "(not initialised)");
    assert_eq!(row(&report, "reference.db").value, "(absent)");
    assert_eq!(row(&report, "queue snapshot").value, "(absent)");
}

#[tokio::test]
async fn gather_carries_a_store_that_will_not_open_as_exactly_one_more_failure() {
    process_env(ProcessEnv::daemon());
    let root = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempfile::tempdir().expect("tempdir");
    let baseline = gather(&selection(root.path()), Some(runtime_dir.path())).await;

    std::fs::write(root.path().join("corpus.db"), b"this is not a database")
        .expect("write a broken store");
    let report = gather(&selection(root.path()), Some(runtime_dir.path())).await;

    let corpus = row(&report, "corpus.db");
    let Status::Fail { note } = &corpus.status else {
        panic!("a store that cannot be opened is a failure: {corpus:?}");
    };
    assert!(!note.is_empty(), "{corpus:?}");
    // The same host, one broken store: the report's verdict moves by
    // that one row and nothing else in it changes shape.
    assert_eq!(
        report.failure_count(),
        baseline.failure_count() + 1,
        "baseline {baseline:?}\nreport {report:?}"
    );
    assert_eq!(labels(&report), labels(&baseline));
}
