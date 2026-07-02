// SPDX-License-Identifier: Apache-2.0

//! Integration test for the pipeline-run lifecycle around the
//! registry-mediated glean path: a default-parameter
//! [`LibraryHandle::glean_paper`] call must open a `glean` row in the
//! paper catalog's `pipeline_runs` registry, stamp its id onto the
//! `node_paper_audit` row, close the run, and refresh the rollup.

use std::future::Future;
use std::path::PathBuf;

use bookrack_catalog::Catalog;
use bookrack_core::ItemKind;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_glean::GleanParams;
use bookrack_ops::registry::LibraryHandle;
use bookrack_ops::{Caller, Ops, PapersPaths};
use bookrack_query::Library;

/// A constant-vector embedder: the dimension probe and the abstract
/// chunks both get the same `dim`-length vector.
struct Fake {
    dim: usize,
}

impl Embedder for Fake {
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
        let (dim, n) = (self.dim, texts.len());
        async move { Ok(vec![vec![0.25f32; dim]; n]) }
    }
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../glean/tests/fixtures/pdf/synthetic_paper_en.pdf")
}

fn pdfium_available() -> bool {
    // The extract crate's PDFium adapter resolves the runtime binary
    // from `BOOKRACK_PDFIUM_LIB`, the executable's directory, or the
    // per-user managed dir. Reading the env var is a cheap proxy for
    // the same chain.
    std::env::var("BOOKRACK_PDFIUM_LIB").is_ok()
        || std::path::Path::new("/usr/local/lib/libpdfium.dylib").exists()
        || std::path::Path::new("/usr/lib/libpdfium.so").exists()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_glean_records_a_pipeline_run_on_the_paper_catalog() {
    if !pdfium_available() {
        eprintln!("pdfium binary not available; skipping glean run-lifecycle test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let corpus_db = dir.path().join("papers_corpus.db");
    let catalog_db = dir.path().join("papers_catalog.db");
    let lancedb_dir = dir.path().join("lancedb_papers");
    let papers_dir = dir.path().join("papers");

    let papers_library = Library::open(
        corpus_db.clone(),
        catalog_db.clone(),
        &lancedb_dir,
        Fake { dim: 8 },
        "fake-model".to_string(),
        5,
        bookrack_glean::CHUNK_VERSION,
    )
    .await
    .expect("open papers library")
    .with_kind(ItemKind::Paper);

    let ops = Ops::catalog_only(
        dir.path().join("corpus.db"),
        dir.path().join("catalog.db"),
        &dir.path().join("lancedb"),
        dir.path().join("books"),
        dir.path().join("backup"),
        Caller::cli(),
    )
    .with_papers(
        papers_library,
        PapersPaths {
            corpus_db,
            catalog_db: catalog_db.clone(),
            lancedb_dir,
            papers_dir,
        },
    );
    let handle = LibraryHandle::new("t", ops);

    // The embed model tag must match the warm library's, or the
    // post-glean store refresh fails its stamp verification. The
    // params otherwise stay at their defaults — in particular
    // `pipeline_run_id: None`, the production shape under test.
    let params = GleanParams {
        embed: bookrack_config::EmbedConfig {
            model: "fake-model".to_string(),
            ..bookrack_config::EmbedConfig::default()
        },
        ..GleanParams::default()
    };
    let report = handle
        .glean_paper(&fixture_path(), &params)
        .await
        .expect("glean must succeed");

    let catalog = Catalog::open(&catalog_db).expect("open paper catalog");
    let runs = catalog
        .list_pipeline_runs(Some("glean"), None)
        .expect("list runs");
    assert_eq!(runs.len(), 1, "one glean invocation registers one run");
    let run = &runs[0];
    assert_eq!(run.status.as_deref(), Some("ok"));
    assert!(run.finished_at.is_some());

    let audit = catalog
        .node_paper_audit(report.intake_id, "paper")
        .expect("read audit")
        .expect("audit row present");
    assert_eq!(
        audit.pipeline_run_id.as_deref(),
        Some(run.pipeline_run_id.as_str()),
        "the audit row must carry the run id the registry opened",
    );

    let summary = catalog
        .pipeline_run_summary(&run.pipeline_run_id)
        .expect("read rollup")
        .expect("rollup row present");
    assert_eq!(summary.n_papers, 1);
    assert_eq!(summary.n_books, 0);
}
