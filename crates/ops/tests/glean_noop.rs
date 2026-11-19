// SPDX-License-Identifier: Apache-2.0

//! When a resubmitted paper is a no-op, and when it is not.
//!
//! Each test gleans one plain-text paper, changes one thing the no-op
//! decision reads, and gleans the same file again through the pipeline
//! entry.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use bookrack_catalog::Catalog;
use bookrack_corpus::{CHUNK_VERSION_KEY, Corpus, NORMALIZE_VERSION_KEY};
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_ops::pipeline::paper::{GleanError, GleanParams, GleanReport, glean_paper};

/// A constant-vector embedder that counts the batches it is asked for.
struct Fake {
    dim: usize,
    calls: AtomicUsize,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            dim: 8,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Embedder for Fake {
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (dim, n) = (self.dim, texts.len());
        async move { Ok(vec![vec![0.25f32; dim]; n]) }
    }
}

const WITH_ABSTRACT: &str = "A Synthetic Paper\n\n\
    Abstract. This synthetic paper reports how a resubmitted source is judged \
    against the stamps its library carries, in enough words to be an abstract.\n\n\
    The first section describes the method in plain terms.\n";

/// One library on disk, and the paper file gleaned into it.
struct Fixture {
    _dir: tempfile::TempDir,
    file: PathBuf,
    lancedb_dir: PathBuf,
    papers_dir: PathBuf,
    corpus: Corpus,
    catalog: Catalog,
    embedder: Fake,
}

impl Fixture {
    /// A library holding nothing, about to glean a paper of `text`.
    fn new(text: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("paper.txt");
        std::fs::write(&file, text).expect("write paper");
        Fixture::over(dir, file)
    }

    /// A library holding nothing, about to glean the paper at `file`.
    fn at(file: PathBuf) -> Fixture {
        Fixture::over(tempfile::tempdir().expect("tempdir"), file)
    }

    fn over(dir: tempfile::TempDir, file: PathBuf) -> Fixture {
        Fixture {
            file,
            lancedb_dir: dir.path().join("lancedb_papers"),
            papers_dir: dir.path().join("papers"),
            corpus: Corpus::open_in_memory().expect("corpus"),
            catalog: Catalog::open_in_memory().expect("catalog"),
            embedder: Fake::new(),
            _dir: dir,
        }
    }

    async fn glean(&mut self) -> Result<GleanReport, GleanError> {
        glean_paper(
            &self.file,
            &mut self.corpus,
            &mut self.catalog,
            &self.lancedb_dir,
            &self.papers_dir,
            &self.embedder,
            &GleanParams::default(),
        )
        .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_paper_under_current_stamps_is_a_noop() {
    let mut fx = Fixture::new(WITH_ABSTRACT);
    let first = fx.glean().await.expect("first glean");
    assert!(!first.no_op);
    assert_eq!(first.chunks_written, 1, "the abstract is one chunk");

    let second = fx.glean().await.expect("second glean");
    assert!(second.no_op, "nothing moved, so the resubmit has no work");
    assert_eq!(second.intake_id, first.intake_id);
    assert_eq!(second.work_node_id, first.work_node_id);
    assert_eq!(
        (
            second.audit_verdict.clone(),
            second.audit_confidence.clone()
        ),
        (first.audit_verdict.clone(), first.audit_confidence.clone()),
        "a no-op reports the verdict and confidence the first run stored",
    );
    assert!(
        second.audit_verdict.is_some() && second.audit_confidence.is_some(),
        "the stored audit outcome is read for a paper, not left empty",
    );
}

/// A corpus stamped under another chunk version holds chunks this
/// build would cut differently: the resubmit is not a no-op, and the
/// stamp gate of the embed stage refuses to write under the mismatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paper_is_not_a_noop_after_the_chunk_version_moves() {
    let mut fx = Fixture::new(WITH_ABSTRACT);
    fx.glean().await.expect("first glean");
    fx.corpus
        .meta_set(CHUNK_VERSION_KEY, "9999")
        .expect("restamp chunk version");

    let second = fx.glean().await;
    assert!(
        !matches!(&second, Ok(report) if report.no_op),
        "a paper chunked under another chunk version was reported unchanged: {second:?}",
    );
    assert!(
        matches!(second, Err(GleanError::Corpus(_))),
        "the rerun reaches the embed stage's stamp gate: {second:?}",
    );
}

/// The same for the normalize version.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paper_is_not_a_noop_after_the_normalize_version_moves() {
    let mut fx = Fixture::new(WITH_ABSTRACT);
    fx.glean().await.expect("first glean");
    fx.corpus
        .meta_set(NORMALIZE_VERSION_KEY, "9999")
        .expect("restamp normalize version");

    let second = fx.glean().await;
    assert!(
        !matches!(&second, Ok(report) if report.no_op),
        "a paper normalized under another version was reported unchanged: {second:?}",
    );
    assert!(
        matches!(second, Err(GleanError::Corpus(_))),
        "the rerun reaches the embed stage's stamp gate: {second:?}",
    );
}

/// A PDF whose first two pages are blank and whose third names no
/// abstract: the pipeline finds no abstract in it.
fn without_abstract() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pdf/synthetic_paper_no_abstract.pdf")
}

/// A paper with no abstract has no chunk, so its first run sends
/// nothing to the embedder and leaves the corpus unstamped. In a
/// library where no paper has been embedded, the resubmit finds no
/// stamps to compare and runs the pipeline again, still without an
/// embed request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paper_without_an_abstract_reruns_while_its_library_is_unstamped() {
    if !bookrack_extract::pdfium_gate::available() {
        eprintln!("pdfium binary not available; skipping");
        return;
    }
    let mut fx = Fixture::at(without_abstract());
    let first = fx.glean().await.expect("first glean");
    assert_eq!(first.chunks_written, 0, "no abstract, so no chunk");
    assert_eq!(first.abstract_source, None);
    assert_eq!(fx.corpus.meta_get(CHUNK_VERSION_KEY).expect("meta"), None);

    let second = fx.glean().await.expect("second glean");
    assert!(
        !second.no_op,
        "an unstamped corpus cannot vouch for the chunks: {second:?}"
    );
    assert!(second.already_registered);
    assert_eq!(second.intake_id, first.intake_id);
    assert_eq!(second.chunks_written, 0);
    assert_eq!(fx.embedder.calls(), 0, "neither run asks for an embedding");
}

/// Once a paper with an abstract has stamped the corpus, the one
/// without is a no-op like any other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paper_without_an_abstract_is_a_noop_once_its_library_is_stamped() {
    if !bookrack_extract::pdfium_gate::available() {
        eprintln!("pdfium binary not available; skipping");
        return;
    }
    let mut fx = Fixture::at(without_abstract());
    let first = fx.glean().await.expect("first glean");
    assert_eq!(first.chunks_written, 0);

    let bare = fx.file.clone();
    fx.file = fx.papers_dir.with_file_name("with_abstract.txt");
    std::fs::write(&fx.file, WITH_ABSTRACT).expect("write paper");
    let stamping = fx.glean().await.expect("glean a paper with an abstract");
    assert_eq!(stamping.chunks_written, 1);

    fx.file = bare;
    let second = fx.glean().await.expect("resubmit");
    assert!(second.no_op, "{second:?}");
    assert_eq!(second.intake_id, first.intake_id);
}
