// SPDX-License-Identifier: Apache-2.0

//! End-to-end test for `glean_paper`: drive the five-stage paper
//! pipeline against the synthetic PDF fixture, check the catalog and
//! corpus state, and prove the no-op and force re-run paths behave as
//! documented.

use std::future::Future;
use std::path::PathBuf;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_glean::{GleanParams, glean_paper};
use bookrack_vectors::ChunkStore;

/// A constant-vector embedder. The pipeline calls `embed_batch`
/// twice: once for the dimension probe and once for the abstract
/// chunks; the same `dim`-length vector serves both.
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pdf/synthetic_paper_en.pdf")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glean_paper_walks_the_five_stage_pipeline_and_is_idempotent() {
    // PDFium runtime dependency: skip cleanly if the binary cannot be
    // loaded, matching the rest of the workspace's PDF tests.
    if !pdfium_available() {
        eprintln!("pdfium binary not available; skipping glean end-to-end test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let papers_dir = dir.path().join("papers");
    let lancedb_dir = dir.path().join("lancedb_papers");
    let mut corpus = Corpus::open_in_memory().expect("corpus");
    let mut catalog = Catalog::open_in_memory().expect("catalog");
    let embedder = Fake { dim: 8 };

    let report = glean_paper(
        &fixture_path(),
        &mut corpus,
        &mut catalog,
        &lancedb_dir,
        &papers_dir,
        &embedder,
        &GleanParams::default(),
    )
    .await
    .expect("glean must succeed");
    assert!(!report.no_op);
    assert!(!report.already_registered);
    assert_eq!(report.nodes_written, 2, "one Work root + one abstract leaf");
    assert!(
        report.chunks_written >= 1,
        "the abstract must produce at least one chunk, got {}",
        report.chunks_written
    );

    // IDENTIFY pass. The synthetic fixture footer carries a DOI, an
    // arXiv id, and a `Proceedings of …` venue line; the abstract
    // sits under a centred `Abstract` heading.
    assert_eq!(
        report.doi.as_deref(),
        Some("10.5555/synthetic.0001"),
        "DOI from the fixture footer must round-trip into the report"
    );
    assert_eq!(
        report.arxiv_id.as_deref(),
        Some("0000.00001"),
        "arXiv id from the fixture footer must round-trip into the report"
    );
    assert!(
        report
            .venue
            .as_deref()
            .is_some_and(|v| v.contains("Proceedings of the Synthetic Conference")),
        "venue cue must capture the Proceedings line, got {:?}",
        report.venue
    );
    assert_eq!(
        report.abstract_source.as_deref(),
        Some("heading"),
        "the heading-anchored abstract path must win on the fixture"
    );

    // The catalog row carries `scope = "paper"` and reaches `Embedded`.
    let attrs = catalog
        .publication_attrs(report.intake_id, ItemKind::Paper)
        .expect("read attrs")
        .expect("present");
    assert_eq!(attrs.scope, "paper");
    assert_eq!(
        attrs.doi.as_deref(),
        Some("10.5555/synthetic.0001"),
        "DOI must be written into the catalog row"
    );
    assert_eq!(attrs.arxiv_id.as_deref(), Some("0000.00001"));
    assert!(
        attrs.abstract_text.is_some(),
        "abstract text must be persisted to the catalog row"
    );
    let bytes = std::fs::read(fixture_path()).expect("read fixture for sha");
    let source_sha = hex_sha256(&bytes);
    let intake = catalog
        .intake_by_sha(&source_sha)
        .expect("read intake")
        .expect("present");
    assert_eq!(intake.status, IntakeStatus::Embedded);

    // The opaque-store envelope landed on disk and the stored_path
    // points at it.
    assert!(intake.stored_path.is_some(), "stored_path must be recorded");
    let stored = intake.stored_path.as_ref().expect("stored");
    assert!(
        std::path::Path::new(stored).exists(),
        "envelope file must exist on disk: {stored}"
    );

    // The vector store has at least one row for this intake.
    let store = ChunkStore::try_open(&lancedb_dir)
        .await
        .expect("open store")
        .expect("store must exist after a successful chunk write");
    let rows = store
        .scan_partition(PartitionIdx::new(report.intake_id))
        .await
        .expect("scan partition");
    assert!(
        !rows.is_empty(),
        "the abstract's chunks must land in the paper vector store"
    );

    // A second call with the same source short-circuits to a no-op
    // because the file is at `Embedded` and the embed model matches.
    let again = glean_paper(
        &fixture_path(),
        &mut corpus,
        &mut catalog,
        &lancedb_dir,
        &papers_dir,
        &embedder,
        &GleanParams::default(),
    )
    .await
    .expect("re-glean must succeed");
    assert!(again.no_op, "re-glean must short-circuit");
    assert_eq!(again.intake_id, report.intake_id);
    assert!(!again.forced);

    // Passing `force = true` bypasses the no-op check and walks the
    // pipeline again. The same intake_id is reused; the run is not a
    // no-op even though the source is unchanged.
    let forced = GleanParams {
        force: true,
        ..GleanParams::default()
    };
    let forced = glean_paper(
        &fixture_path(),
        &mut corpus,
        &mut catalog,
        &lancedb_dir,
        &papers_dir,
        &embedder,
        &forced,
    )
    .await
    .expect("forced re-glean must succeed");
    assert!(forced.forced);
    assert!(!forced.no_op);
    assert_eq!(forced.intake_id, report.intake_id);
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
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
