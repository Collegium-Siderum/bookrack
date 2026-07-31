// SPDX-License-Identifier: Apache-2.0

//! Read ops must not create store files.
//!
//! Every test runs a read op against a tempdir-backed data root and
//! asserts on the directory afterwards. The contract is about `.db`
//! files and the lancedb directory — a read-only connection to an
//! existing WAL database may still create `-shm` / `-wal` sidecars,
//! so byte-identical directories are deliberately not required.

use std::path::{Path, PathBuf};

use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::reads::books::show_stats;
use bookrack_ops::reads::info::{LibraryInfoContext, show_library_info};
use bookrack_ops::reads::vectors::status as vectors_status;
use bookrack_ops::{Caller, Ops};
use tempfile::TempDir;

struct Fixture {
    tmp: TempDir,
    ops: Ops<OllamaEmbedClient>,
}

impl Fixture {
    /// An `Ops` over a data root that contains nothing at all.
    fn empty_root() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            tmp.path().join("corpus.db"),
            tmp.path().join("catalog.db"),
            &tmp.path().join("lancedb"),
            tmp.path().join("books"),
            tmp.path().join("backup"),
            Caller::cli(),
        );
        Fixture { tmp, ops }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn info_ctx(&self) -> LibraryInfoContext {
        LibraryInfoContext {
            data_dir: self.root().display().to_string(),
            library_name: None,
            resolution_source: "test".to_string(),
            shadowed_default: None,
            library_identification: None,
            ollama_url: "http://localhost:0/".to_string(),
            embed_model_configured: "test-model".to_string(),
            mcp_addr: String::new(),
        }
    }
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read data root")
        .map(|e| e.expect("dir entry").path())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn library_info_on_an_empty_root_reports_absence_and_creates_nothing() {
    let fx = Fixture::empty_root();
    let info = show_library_info(&fx.ops, fx.info_ctx())
        .await
        .expect("info on an empty root");

    // A fresh root is a clean absence, not an error.
    assert!(info.catalog_error.is_none(), "{:?}", info.catalog_error);
    assert!(info.corpus_error.is_none(), "{:?}", info.corpus_error);
    assert!(info.vectors_error.is_none(), "{:?}", info.vectors_error);
    assert!(info.catalog_schema_version_on_disk.is_none());
    assert!(info.intake_count.is_none());

    assert_eq!(entries(fx.root()), Vec::<PathBuf>::new());
}

#[tokio::test]
async fn vectors_status_on_an_empty_root_reports_empty_and_creates_nothing() {
    let fx = Fixture::empty_root();
    let status = vectors_status(&fx.ops).await.expect("status");
    assert!(status.row_count.is_none());
    assert!(status.indices.is_empty());
    assert_eq!(entries(fx.root()), Vec::<PathBuf>::new());
}

#[test]
fn stats_on_an_empty_root_fails_without_creating_the_catalog() {
    let fx = Fixture::empty_root();
    show_stats(&fx.ops).expect_err("no catalog to aggregate");
    assert_eq!(entries(fx.root()), Vec::<PathBuf>::new());
}

#[tokio::test]
async fn library_info_reports_unreadable_stores_instead_of_posing_as_missing() {
    let fx = Fixture::empty_root();
    // Files that exist but are not SQLite databases: the health
    // signal must survive into the error fields.
    std::fs::write(fx.root().join("catalog.db"), b"not a database").expect("catalog garbage");
    std::fs::write(fx.root().join("corpus.db"), b"not a database").expect("corpus garbage");

    let info = show_library_info(&fx.ops, fx.info_ctx())
        .await
        .expect("info stays informational on a broken library");
    assert!(info.catalog_error.is_some(), "catalog_error must be set");
    assert!(info.corpus_error.is_some(), "corpus_error must be set");
    // The count-class fields stay absent rather than failing the call.
    assert!(info.intake_count.is_none());
}
