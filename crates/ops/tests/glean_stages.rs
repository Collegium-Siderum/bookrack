// SPDX-License-Identifier: Apache-2.0

//! The two stages the paper pipeline shares with the book pipeline,
//! seen through the paper entry point: the decision that a resubmitted
//! source needs no rerun, and the embed stage.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use bookrack_catalog::Catalog;
use bookrack_core::PartitionIdx;
use bookrack_corpus::{CHUNK_VERSION_KEY, Corpus, NORMALIZE_VERSION_KEY};
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_ops::pipeline::paper::{
    GleanError, GleanParams, GleanReport, glean_paper, glean_paper_with_progress,
};
use bookrack_vectors::ChunkReader;

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
        self.glean_with(&GleanParams::default()).await
    }

    async fn glean_with(&mut self, params: &GleanParams) -> Result<GleanReport, GleanError> {
        glean_paper(
            &self.file,
            &mut self.corpus,
            &mut self.catalog,
            &self.lancedb_dir,
            &self.papers_dir,
            &self.embedder,
            params,
        )
        .await
    }

    /// Glean, returning every `(consumed, total)` the embed stage
    /// reported beside the outcome.
    async fn glean_reporting(&mut self) -> (Result<GleanReport, GleanError>, Vec<(usize, usize)>) {
        let seen = Mutex::new(Vec::new());
        let outcome = glean_paper_with_progress(
            &self.file,
            &mut self.corpus,
            &mut self.catalog,
            &self.lancedb_dir,
            &self.papers_dir,
            &self.embedder,
            &GleanParams::default(),
            &|consumed, total| seen.lock().expect("lock").push((consumed, total)),
        )
        .await;
        (outcome, seen.into_inner().expect("lock"))
    }

    /// Point the fixture at a second paper of `text`.
    fn another_paper(&mut self, name: &str, text: &str) {
        self.file = self.papers_dir.with_file_name(name);
        std::fs::write(&self.file, text).expect("write paper");
    }

    fn vectors_meta(&self) -> bookrack_vectors::meta::VectorsMeta {
        bookrack_vectors::meta::load(&self.lancedb_dir)
            .expect("read vectors_meta.json")
            .expect("vectors_meta.json is present")
    }

    async fn rows(&self) -> usize {
        ChunkReader::open(&self.lancedb_dir)
            .await
            .expect("open store")
            .expect("chunks table")
            .count_rows()
            .await
            .expect("count")
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
    let meta_path = fx.lancedb_dir.join("vectors_meta.json");
    let meta_before = std::fs::read(&meta_path).expect("the first glean wrote vectors_meta.json");
    let calls_before = fx.embedder.calls();

    let second = fx.glean().await;
    assert_eq!(
        fx.embedder.calls(),
        calls_before,
        "the stamp gate refuses before the embedder is asked for anything",
    );
    assert!(
        !matches!(&second, Ok(report) if report.no_op),
        "a paper chunked under another chunk version was reported unchanged: {second:?}",
    );
    assert!(
        matches!(second, Err(GleanError::Corpus(_))),
        "the rerun reaches the embed stage's stamp gate: {second:?}",
    );
    assert_eq!(
        std::fs::read(&meta_path).expect("a refused write leaves vectors_meta.json in place"),
        meta_before,
        "a refused write does not touch the index metadata",
    );
    assert_eq!(fx.rows().await, 1, "nor the rows it describes");
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

const SECOND_PAPER: &str = "Another Synthetic Paper\n\n\
    Abstract. This second synthetic paper exists to add one more chunk to a \
    store that already holds one, in enough words to be taken as an abstract.\n";

/// The embed stage clears the partition its first chunk lives in. A
/// paper's chunks live in the partition its intake id names, so a
/// rerun replaces that paper's rows and no other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rerun_replaces_the_rows_of_its_own_paper_only() {
    let mut fx = Fixture::new(WITH_ABSTRACT);
    let first = fx.glean().await.expect("first glean");
    assert_eq!(
        first.work_node_id.partition(),
        PartitionIdx::new(first.intake_id)
    );
    let scan = |dir: PathBuf, intake_id: i64| async move {
        ChunkReader::open(&dir)
            .await
            .expect("open store")
            .expect("chunks table")
            .scan_partition(PartitionIdx::new(intake_id))
            .await
            .expect("scan")
    };
    let rows = scan(fx.lancedb_dir.clone(), first.intake_id).await;
    assert_eq!(
        rows.len(),
        1,
        "the abstract chunk is in the intake's partition"
    );
    assert_eq!(
        rows[0].start_node_id.partition(),
        PartitionIdx::new(first.intake_id)
    );

    let own = fx.file.clone();
    fx.another_paper("second.txt", SECOND_PAPER);
    let second = fx.glean().await.expect("second paper");
    assert_ne!(second.intake_id, first.intake_id);
    assert_eq!(fx.rows().await, 2);

    fx.file = own;
    let forced = fx
        .glean_with(&GleanParams {
            force: true,
            ..GleanParams::default()
        })
        .await
        .expect("forced rerun");
    assert_eq!(forced.intake_id, first.intake_id);
    assert_eq!(forced.chunks_written, 1);
    assert_eq!(
        fx.rows().await,
        2,
        "the rerun replaced one row and kept the other"
    );
    assert_eq!(scan(fx.lancedb_dir.clone(), first.intake_id).await.len(), 1);
    assert_eq!(
        scan(fx.lancedb_dir.clone(), second.intake_id).await.len(),
        1
    );
}

/// The first paper into a store builds its index and writes
/// `vectors_meta.json`; later runs count their churn in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_paper_builds_the_index_and_later_runs_count_churn() {
    let mut fx = Fixture::new(WITH_ABSTRACT);
    assert!(
        bookrack_vectors::meta::load(&fx.lancedb_dir)
            .expect("read")
            .is_none()
    );
    fx.glean().await.expect("first glean");
    let meta = fx.vectors_meta();
    assert_eq!(meta.kind, "ivf-flat");
    assert_eq!(meta.num_partitions, 1, "one row trains one partition");
    assert_eq!(meta.built_at_chunk_count, 1);
    assert_eq!(meta.churn_since_rebuild, 0);
    let reader = ChunkReader::open(&fx.lancedb_dir)
        .await
        .expect("open store")
        .expect("chunks table");
    assert_eq!(reader.list_indices().await.expect("indices").len(), 1);

    fx.another_paper("second.txt", SECOND_PAPER);
    fx.glean().await.expect("second paper");
    assert_eq!(fx.vectors_meta().churn_since_rebuild, 1, "one row appended");

    fx.glean_with(&GleanParams {
        force: true,
        ..GleanParams::default()
    })
    .await
    .expect("forced rerun");
    let meta = fx.vectors_meta();
    assert_eq!(
        meta.churn_since_rebuild, 3,
        "one row deleted and one appended"
    );
    assert_eq!(
        meta.built_at_chunk_count, 1,
        "churn this small retrains nothing"
    );
}

/// A store whose index was dropped on purpose stays without one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_index_is_not_rebuilt_by_the_next_paper() {
    let mut fx = Fixture::new(WITH_ABSTRACT);
    fx.glean().await.expect("first glean");
    let store = bookrack_vectors::ChunkStore::open_for_write(&fx.lancedb_dir, 8)
        .await
        .expect("open store");
    store
        .drop_ann_index(&fx.lancedb_dir, "2000-01-01T00:00:00Z".to_string())
        .await
        .expect("drop index");
    drop(store);
    assert_eq!(fx.vectors_meta().kind, "brute-force");

    fx.another_paper("second.txt", SECOND_PAPER);
    fx.glean().await.expect("second paper");
    assert_eq!(fx.vectors_meta().kind, "brute-force");
    let reader = ChunkReader::open(&fx.lancedb_dir)
        .await
        .expect("open store")
        .expect("chunks table");
    assert!(reader.list_indices().await.expect("indices").is_empty());
}

/// The embed stage reports a paper's batches, and only when there is
/// something to embed: a no-op resubmit reports nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paper_with_chunks_reports_its_batches_and_a_noop_reports_none() {
    let mut fx = Fixture::new(WITH_ABSTRACT);
    let (first, batches) = fx.glean_reporting().await;
    assert_eq!(first.expect("first glean").chunks_written, 1);
    assert_eq!(
        batches,
        vec![(1, 1)],
        "one chunk, one batch, ending at the total"
    );

    let (second, batches) = fx.glean_reporting().await;
    assert!(second.expect("resubmit").no_op);
    assert_eq!(batches, vec![], "a no-op never enters the embed stage");
}

/// A paper with no abstract has nothing to embed and reports no batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paper_without_an_abstract_reports_no_batch() {
    if !bookrack_extract::pdfium_gate::available() {
        eprintln!("pdfium binary not available; skipping");
        return;
    }
    let mut fx = Fixture::at(without_abstract());
    let (report, batches) = fx.glean_reporting().await;
    assert_eq!(report.expect("glean").chunks_written, 0);
    assert_eq!(batches, vec![]);
    assert!(
        bookrack_vectors::meta::load(&fx.lancedb_dir)
            .expect("read")
            .is_none(),
        "nothing was written, so no index is declared"
    );
}
