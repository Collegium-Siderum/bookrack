// SPDX-License-Identifier: Apache-2.0

//! End-to-end test for `glean_paper`: drive the five-stage paper
//! pipeline against the synthetic PDF fixture, check the catalog and
//! corpus state, and prove the no-op and force re-run paths behave as
//! documented.

use std::future::Future;
use std::path::PathBuf;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_core::{ItemKind, NodeType, PartitionIdx};
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
    // After Phase 1: one Work root + one abstract leaf + one Paragraph
    // leaf per non-empty BlockKind::Body block. The exact body count
    // depends on the extractor's segmentation, so the test reads the
    // tree from corpus rather than locking a magic number.
    let work_root = report.work_node_id;
    let leaves = corpus
        .leaves_in_doc_span(work_root, 0, i64::from(i32::MAX), 4096)
        .expect("leaves");
    assert!(
        leaves.len() >= 2,
        "abstract leaf + at least one body leaf, got {} leaves",
        leaves.len()
    );
    assert!(
        leaves
            .iter()
            .all(|n| matches!(n.node_type, NodeType::Paragraph)),
        "every leaf must be a Paragraph"
    );
    assert_eq!(
        report.nodes_written,
        1 + leaves.len(),
        "nodes_written counts the Work root plus every leaf"
    );
    // First leaf is the abstract; its stable_anchor namespace ends in
    // `:abstract` and it carries no source-page bounds. Body leaves
    // use `:body:{i}` and carry source_unit page bounds.
    assert!(
        leaves[0]
            .stable_anchor
            .as_deref()
            .is_some_and(|a| a.ends_with(":abstract")),
        "first leaf must be the abstract anchor, got {:?}",
        leaves[0].stable_anchor
    );
    assert!(
        leaves[0].page_index_start.is_none() && leaves[0].page_index_end.is_none(),
        "abstract leaf carries no page bounds — pre-Phase-1 shape"
    );
    for (i, leaf) in leaves.iter().skip(1).enumerate() {
        let expected = format!(":body:{i}");
        assert!(
            leaf.stable_anchor
                .as_deref()
                .is_some_and(|a| a.ends_with(&expected)),
            "body leaf {i} anchor mismatch: {:?}",
            leaf.stable_anchor
        );
        assert!(
            leaf.page_index_start.is_some() && leaf.page_index_end.is_some(),
            "body leaf {i} must carry source_unit page bounds"
        );
    }
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
        Some("heading-en"),
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

    // Phase 0: the source PDF's bytes are archived alongside the
    // envelope with `paper-<id>.pdf` as the file name, and
    // `intake.source_pdf_path` carries the canonical absolute path.
    let archive_path = papers_dir.join(format!("paper-{}.pdf", report.intake_id));
    assert!(
        archive_path.exists(),
        "source PDF copy must land at {archive_path:?}"
    );
    let archived_bytes = std::fs::read(&archive_path).expect("read archived PDF");
    assert_eq!(
        hex_sha256(&archived_bytes),
        source_sha,
        "archived bytes must hash to the source SHA-256",
    );
    let expected_archive_path = archive_path
        .canonicalize()
        .expect("canonicalize archive path");
    assert_eq!(
        intake.source_pdf_path.as_deref().map(std::path::Path::new),
        Some(expected_archive_path.as_path()),
        "intake.source_pdf_path must point at the archived bytes"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glean_paper_skips_source_pdf_archive_when_disabled() {
    if !pdfium_available() {
        eprintln!("pdfium binary not available; skipping glean keep-disabled test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let papers_dir = dir.path().join("papers");
    let lancedb_dir = dir.path().join("lancedb_papers");
    let mut corpus = Corpus::open_in_memory().expect("corpus");
    let mut catalog = Catalog::open_in_memory().expect("catalog");
    let embedder = Fake { dim: 8 };

    let params = GleanParams {
        keep_source_pdf: false,
        ..GleanParams::default()
    };
    let report = glean_paper(
        &fixture_path(),
        &mut corpus,
        &mut catalog,
        &lancedb_dir,
        &papers_dir,
        &embedder,
        &params,
    )
    .await
    .expect("glean must succeed with keep_source_pdf = false");

    // The archive file is not created and the column stays NULL —
    // `fetch_source` returning `SourceNotArchived` is the contract.
    let archive_path = papers_dir.join(format!("paper-{}.pdf", report.intake_id));
    assert!(
        !archive_path.exists(),
        "source PDF must not be archived when the param is disabled"
    );
    let bytes = std::fs::read(fixture_path()).expect("read fixture for sha");
    let source_sha = hex_sha256(&bytes);
    let intake = catalog
        .intake_by_sha(&source_sha)
        .expect("read intake")
        .expect("present");
    assert_eq!(intake.source_pdf_path, None);

    // The rest of the REGISTER -> EMBED progression is unchanged: the
    // envelope still lands and the intake still reaches `Embedded`.
    assert!(intake.stored_path.is_some());
    assert_eq!(intake.status, IntakeStatus::Embedded);
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
