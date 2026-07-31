// SPDX-License-Identifier: Apache-2.0

//! Integration test for the pipeline-run lifecycle around
//! `bookrack distill build`: an invocation must open a `distill_build`
//! row in the catalog's `pipeline_runs` registry, stamp its id onto
//! every `book_distill_audit` row the invocation writes, close the run
//! with a terminal status, and refresh the rollup.
//!
//! The entry point is [`bookrack_cli::distill_cmd::run`], driven with
//! an explicit `--data-dir` selection so configuration resolution runs
//! for real against a throwaway data root and no environment variable
//! is touched.

use std::fs;
use std::path::{Path, PathBuf};

use bookrack_catalog::{Catalog, GATE_STATUS_FAIL, GATE_STATUS_PASS};
use bookrack_cli_grammar::{DistillAction, DistillBuildArgs};
use bookrack_config::LibrarySelection;
use tempfile::TempDir;

/// A two-headword book whose pipeline exercises every stage kind the
/// distill runner reports on, kept minimal so the test asserts on the
/// run lifecycle rather than on extraction quality.
const TINY_BOOK_TOML: &str = r#"
book_slug      = "tiny"
schema_name    = "name_translation"
schema_version = 1
parser_version = "0.1.0"
authority_rank = 10

[parser]
writes_properties = []
stages = [
  "split_pages",
  { stage = "one_block_per_page", lang = "latin" },
  { stage = "walk_anchors",
    anchor = "latin_headword",
    splice_orphans_to_prev_block = false },
  "split_headline_only",
  { stage = "to_entry_draft",
    key_normalizer = "normalize_latin_key" },
]
"#;

const TINY_SOURCE: &str = "<!-- page 1 (sheet 1) -->\nSmith\nJones\n";

fn seed_book_dir(root: &Path, slug: &str) -> PathBuf {
    let book_dir = root.join("reference").join(slug);
    fs::create_dir_all(&book_dir).expect("mkdir");
    let toml = TINY_BOOK_TOML.replace("\"tiny\"", &format!("\"{slug}\""));
    fs::write(book_dir.join("book.toml"), toml).expect("write book.toml");
    fs::write(book_dir.join("source.md"), TINY_SOURCE).expect("write source.md");
    book_dir
}

fn selection(root: &Path) -> LibrarySelection {
    LibrarySelection {
        data_dir: Some(root.to_path_buf()),
        library: None,
    }
}

fn build_args(paths: Vec<PathBuf>) -> DistillBuildArgs {
    DistillBuildArgs {
        paths,
        recursive: false,
        dry_run: false,
        retention_threshold: 0.10,
        no_retention_check: false,
        no_audit_write: false,
    }
}

/// Run `distill build` through the public subcommand entry point.
async fn distill_build(root: &Path, args: DistillBuildArgs) -> eyre::Result<()> {
    bookrack_cli::distill_cmd::run(&selection(root), DistillAction::Build(args)).await
}

fn catalog_path(root: &Path) -> PathBuf {
    root.join("catalog.db")
}

#[tokio::test]
async fn distill_build_writes_audit_with_pipeline_run_id() {
    let tmp = TempDir::new().expect("tmp");
    let book_dir = seed_book_dir(tmp.path(), "tiny");

    distill_build(tmp.path(), build_args(vec![book_dir]))
        .await
        .expect("build");

    let catalog = Catalog::open(&catalog_path(tmp.path())).expect("open catalog");
    let runs = catalog
        .list_pipeline_runs(Some("distill_build"), None)
        .expect("list runs");
    assert_eq!(runs.len(), 1, "one build invocation registers one run");
    let run = &runs[0];
    assert_eq!(run.status.as_deref(), Some("ok"));
    assert!(run.finished_at.is_some(), "a closed run stamps finished_at");
    assert_eq!(
        run.library_root.as_deref(),
        tmp.path().to_str(),
        "the run is attributed to the data root that owns catalog.db",
    );

    let rows = catalog
        .distill_audits_for_book("tiny")
        .expect("read audits");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].gate_status, GATE_STATUS_PASS);
    assert_eq!(
        rows[0].pipeline_run_id.as_deref(),
        Some(run.pipeline_run_id.as_str()),
        "the audit row must carry the run id the build opened",
    );

    let summary = catalog
        .pipeline_run_summary(&run.pipeline_run_id)
        .expect("read rollup")
        .expect("rollup row present");
    assert_eq!(summary.n_books, 1);
    assert_eq!(summary.n_papers, 0);
}

#[tokio::test]
async fn one_invocation_opens_one_run_for_all_books() {
    let tmp = TempDir::new().expect("tmp");
    let alpha = seed_book_dir(tmp.path(), "alpha");
    let beta = seed_book_dir(tmp.path(), "beta");

    distill_build(tmp.path(), build_args(vec![alpha, beta]))
        .await
        .expect("build");

    let catalog = Catalog::open(&catalog_path(tmp.path())).expect("open catalog");
    let runs = catalog
        .list_pipeline_runs(Some("distill_build"), None)
        .expect("list runs");
    assert_eq!(runs.len(), 1, "the run is per invocation, not per book");
    let run_id = runs[0].pipeline_run_id.as_str();

    for slug in ["alpha", "beta"] {
        let rows = catalog.distill_audits_for_book(slug).expect("read audits");
        assert_eq!(rows.len(), 1, "{slug} writes one audit row");
        assert_eq!(
            rows[0].pipeline_run_id.as_deref(),
            Some(run_id),
            "{slug} must share the invocation's run id",
        );
    }

    let summary = catalog
        .pipeline_run_summary(run_id)
        .expect("read rollup")
        .expect("rollup row present");
    assert_eq!(summary.n_books, 2, "the rollup counts both books");
}

#[tokio::test]
async fn failed_build_closes_the_run_with_error_status() {
    let tmp = TempDir::new().expect("tmp");
    let book_dir = seed_book_dir(tmp.path(), "tiny");

    // An out-of-range threshold trips the retention guard's validity
    // check, so the build bails after the audit row lands.
    let mut args = build_args(vec![book_dir]);
    args.retention_threshold = 1.5;
    let err = distill_build(tmp.path(), args)
        .await
        .expect_err("retention must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("threshold"),
        "the run must fail on the retention guard, not elsewhere: {msg}",
    );

    let catalog = Catalog::open(&catalog_path(tmp.path())).expect("open catalog");
    let runs = catalog
        .list_pipeline_runs(Some("distill_build"), None)
        .expect("list runs");
    assert_eq!(runs.len(), 1, "a failed build still registers its run");
    let run = &runs[0];
    assert_eq!(
        run.status.as_deref(),
        Some("error"),
        "a build that bails closes its run as error, not ok",
    );
    assert!(run.finished_at.is_some(), "a failed run is still closed");

    let rows = catalog
        .distill_audits_for_book("tiny")
        .expect("read audits");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].gate_status, GATE_STATUS_FAIL);
    assert_eq!(
        rows[0].pipeline_run_id.as_deref(),
        Some(run.pipeline_run_id.as_str()),
        "the gate-failure row is attributed to the run like any other",
    );
}

#[tokio::test]
async fn dry_run_still_registers_the_run_and_stamps_the_audit_row() {
    let tmp = TempDir::new().expect("tmp");
    let book_dir = seed_book_dir(tmp.path(), "tiny");

    let mut args = build_args(vec![book_dir]);
    args.dry_run = true;
    distill_build(tmp.path(), args)
        .await
        .expect("dry-run build");

    let catalog = Catalog::open(&catalog_path(tmp.path())).expect("open catalog");
    let runs = catalog
        .list_pipeline_runs(Some("distill_build"), None)
        .expect("list runs");
    assert_eq!(runs.len(), 1, "a dry run is an observation worth recording");
    let run = &runs[0];
    assert_eq!(run.status.as_deref(), Some("ok"));

    let rows = catalog
        .distill_audits_for_book("tiny")
        .expect("read audits");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].pipeline_run_id.as_deref(),
        Some(run.pipeline_run_id.as_str()),
    );
}

#[tokio::test]
async fn no_audit_write_opens_no_run() {
    let tmp = TempDir::new().expect("tmp");
    let book_dir = seed_book_dir(tmp.path(), "tiny");

    let mut args = build_args(vec![book_dir]);
    args.no_audit_write = true;
    distill_build(tmp.path(), args).await.expect("build");

    assert!(
        !catalog_path(tmp.path()).exists(),
        "--no-audit-write must not open a run, and so must not create catalog.db",
    );
}
