// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the five search ops: the backend-availability
//! contracts (`SearchUnavailable`, `PapersBackendNotConfigured`,
//! `IntakeNotFound`), the unified cross-store merge, and the reranker
//! stage wiring behind [`Ops::with_reranker`].
//!
//! Every fixture is file-backed and searched through a stub embedder;
//! the reranker stage talks to a loopback HTTP mock. No Ollama, no
//! llama-server, no PDFium.

use std::future::Future;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bookrack_catalog::{Catalog, NewIntake};
use bookrack_core::{ItemKind, NodeType};
use bookrack_corpus::Corpus;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_extract::{Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc};
use bookrack_ingest::{StructureParams, current_index_stamps, ingest_structure};
use bookrack_ops::reads::search::{
    search, search_in_book, search_in_paper, search_paper, search_unified,
};
use bookrack_ops::{Caller, Ops, OpsError, PapersPaths, RerankStage};
use bookrack_query::{Library, SearchOptions};
use bookrack_rerank::RerankClient;
use bookrack_vectors::{ChunkRow, ChunkStore};
use tempfile::TempDir;

const DIM: usize = 4;
const MODEL: &str = "test-model";

/// An embedder that returns one fixed vector per input, ignoring the
/// text. Serves both the dimension probe and the query embedding.
#[derive(Clone)]
struct Fixed {
    vector: Vec<f32>,
}

impl Fixed {
    fn unit_x() -> Fixed {
        Fixed {
            vector: vec![1.0, 0.0, 0.0, 0.0],
        }
    }
}

impl Embedder for Fixed {
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
        let vector = self.vector.clone();
        let n = texts.len();
        async move { Ok(vec![vector; n]) }
    }
}

/// A one-leaf extraction whose body is `text`.
fn extraction(text: &str) -> Extraction {
    Extraction {
        blocks: vec![Block {
            kind: BlockKind::Body,
            text: text.to_string(),
            source_unit: 0,
            style: None,
        }],
        toc: Toc {
            entries: Vec::new(),
        },
        biblio: Biblio::default(),
        provenance: Provenance {
            adapter: "test".to_string(),
            extractor_version: 1,
            text_layer_quality: TextLayerQuality::BornDigital,
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks: Vec::new(),
        },
    }
}

/// One file-backed store: catalog with a registered intake, a stamped
/// corpus holding one single-leaf book, and a chunk per
/// `(text, vector)` pair, all under the same tempdir subtree.
struct StorePaths {
    corpus_db: PathBuf,
    catalog_db: PathBuf,
    lancedb_dir: PathBuf,
}

async fn seed_store(root: &Path, name: &str, chunks: &[(&str, Vec<f32>)]) -> StorePaths {
    let corpus_db = root.join(format!("{name}-corpus.db"));
    let catalog_db = root.join(format!("{name}-catalog.db"));
    let lancedb_dir = root.join(format!("{name}-lancedb"));

    let catalog = Catalog::open(&catalog_db).expect("seed catalog");
    let mut catalog = catalog;
    catalog
        .register_intake(ItemKind::Book, &NewIntake::new(format!("sha-{name}")))
        .expect("register intake");

    let mut corpus = Corpus::open(&corpus_db).expect("seed corpus");
    let report = ingest_structure(
        &mut corpus,
        1,
        NodeType::Work,
        &extraction("The passage body."),
        &StructureParams::default(),
    )
    .expect("structure");
    corpus
        .reconcile_index_stamps(&current_index_stamps(MODEL, DIM as u32))
        .expect("stamp");
    let leaf = corpus
        .book_nodes(report.book_root_id)
        .expect("nodes")
        .into_iter()
        .find(|n| n.node_type.is_prose_leaf())
        .expect("a prose leaf");

    let store = ChunkStore::open(&lancedb_dir, DIM).await.expect("store");
    let rows: Vec<ChunkRow> = chunks
        .iter()
        .enumerate()
        .map(|(i, (text, vector))| ChunkRow {
            vector: vector.clone(),
            text: text.to_string(),
            start_node_id: leaf.node_id,
            start_char_offset: 0,
            end_node_id: leaf.node_id,
            end_char_offset: text.len() as i32,
            norm_chunk_sha256: format!("sha-{name}-{i}"),
        })
        .collect();
    if !rows.is_empty() {
        store.append(&rows).await.expect("append chunks");
    }

    StorePaths {
        corpus_db,
        catalog_db,
        lancedb_dir,
    }
}

async fn open_library(paths: &StorePaths, embedder: Fixed) -> Library<Fixed> {
    Library::open(
        paths.corpus_db.clone(),
        paths.catalog_db.clone(),
        &paths.lancedb_dir,
        embedder,
        MODEL.to_string(),
        5,
        bookrack_ingest::CHUNK_VERSION,
    )
    .await
    .expect("open library")
}

fn ops_over(tmp: &TempDir, paths: &StorePaths, library: Library<Fixed>) -> Ops<Fixed> {
    Ops::with_library(
        library,
        paths.corpus_db.clone(),
        paths.catalog_db.clone(),
        &paths.lancedb_dir,
        tmp.path().join("books"),
        tmp.path().join("backup"),
        Caller::cli(),
    )
}

fn attach_papers(
    ops: Ops<Fixed>,
    tmp: &TempDir,
    paths: &StorePaths,
    library: Library<Fixed>,
) -> Ops<Fixed> {
    ops.with_papers(
        library,
        PapersPaths {
            corpus_db: paths.corpus_db.clone(),
            catalog_db: paths.catalog_db.clone(),
            lancedb_dir: paths.lancedb_dir.clone(),
            papers_dir: tmp.path().join("papers"),
        },
    )
}

/// Spawn a loopback `/v1/rerank` mock on a std thread: every request
/// is answered with `body`. Returns the base URL.
fn rerank_mock(body: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind rerank mock");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut out = stream;
            let mut content_length = 0usize;
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let header = header.trim();
                if header.is_empty() {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut request_body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut request_body);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = out.write_all(response.as_bytes());
            let _ = out.flush();
        }
    });
    url
}

/// An address with no listener — connecting to it is refused.
fn dead_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    format!("http://{addr}")
}

fn stage_at(url: &str, top_k_in: usize, top_k_out: usize) -> RerankStage {
    RerankStage {
        client: Arc::new(
            RerankClient::new(
                url,
                "rerank-model",
                Duration::from_secs(5),
                0,
                Duration::from_millis(1),
            )
            .expect("client builds"),
        ),
        top_k_in,
        top_k_out,
    }
}

#[tokio::test]
async fn catalog_only_search_ops_report_their_missing_backend() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let catalog_db = tmp.path().join("catalog.db");
    let corpus_db = tmp.path().join("corpus.db");
    drop(Catalog::open(&catalog_db).expect("seed catalog"));
    drop(Corpus::open(&corpus_db).expect("seed corpus"));
    let ops = Ops::<Fixed>::catalog_only(
        corpus_db,
        catalog_db,
        &tmp.path().join("lancedb"),
        tmp.path().join("books"),
        tmp.path().join("backup"),
        Caller::cli(),
    );

    let err = search(&ops, "q", SearchOptions::default(), None)
        .await
        .expect_err("no warm library");
    assert!(matches!(err, OpsError::SearchUnavailable), "{err:?}");
    let err = search_in_book(&ops, 1, "q", SearchOptions::default(), None)
        .await
        .expect_err("no warm library");
    assert!(matches!(err, OpsError::SearchUnavailable), "{err:?}");
    let err = search_paper(&ops, "q", SearchOptions::default(), None)
        .await
        .expect_err("no papers backend");
    assert!(
        matches!(err, OpsError::PapersBackendNotConfigured),
        "{err:?}"
    );
    let err = search_in_paper(&ops, 1, "q", SearchOptions::default(), None)
        .await
        .expect_err("no papers backend");
    assert!(
        matches!(err, OpsError::PapersBackendNotConfigured),
        "{err:?}"
    );
    let err = search_unified(&ops, "q", SearchOptions::default(), None)
        .await
        .expect_err("no warm library");
    assert!(matches!(err, OpsError::SearchUnavailable), "{err:?}");
}

#[tokio::test]
async fn search_in_book_reports_an_unknown_intake() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = seed_store(
        tmp.path(),
        "books",
        &[("the one passage", vec![1.0, 0.0, 0.0, 0.0])],
    )
    .await;
    let library = open_library(&paths, Fixed::unit_x()).await;
    let ops = ops_over(&tmp, &paths, library);

    // The registered intake answers…
    let hits = search_in_book(&ops, 1, "q", SearchOptions::default(), None)
        .await
        .expect("known intake");
    assert_eq!(hits.len(), 1);

    // …an unregistered one is refused by the catalog check, without a
    // vector roundtrip.
    let err = search_in_book(&ops, 999, "q", SearchOptions::default(), None)
        .await
        .expect_err("unknown intake");
    assert!(
        matches!(err, OpsError::IntakeNotFound { intake_id: 999 }),
        "{err:?}"
    );
}

#[tokio::test]
async fn search_in_paper_reports_an_unknown_intake() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let book_paths = seed_store(tmp.path(), "books", &[]).await;
    let paper_paths = seed_store(
        tmp.path(),
        "papers",
        &[("the abstract", vec![1.0, 0.0, 0.0, 0.0])],
    )
    .await;
    let book_library = open_library(&book_paths, Fixed::unit_x()).await;
    let paper_library = open_library(&paper_paths, Fixed::unit_x())
        .await
        .with_kind(ItemKind::Paper);
    let ops = attach_papers(
        ops_over(&tmp, &book_paths, book_library),
        &tmp,
        &paper_paths,
        paper_library,
    );

    let hits = search_in_paper(&ops, 1, "q", SearchOptions::default(), None)
        .await
        .expect("known paper intake");
    assert_eq!(hits.len(), 1);
    let err = search_in_paper(&ops, 999, "q", SearchOptions::default(), None)
        .await
        .expect_err("unknown paper intake");
    assert!(
        matches!(err, OpsError::IntakeNotFound { intake_id: 999 }),
        "{err:?}"
    );
}

#[tokio::test]
async fn search_unified_orders_across_stores_and_truncates_to_top_k() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Four candidates at distinct angles to the query axis [0,1,0,0]:
    // book-near < paper-near < the two far chunks the top_k=2 cut
    // must drop.
    let book_paths = seed_store(
        tmp.path(),
        "books",
        &[
            ("book far", vec![1.0, 0.0, 0.0, 0.0]),
            ("book near", vec![0.1, 0.9, 0.0, 0.0]),
        ],
    )
    .await;
    let paper_paths = seed_store(
        tmp.path(),
        "papers",
        &[
            ("paper far", vec![1.0, 0.0, 0.0, 0.0]),
            ("paper near", vec![0.5, 0.5, 0.0, 0.0]),
        ],
    )
    .await;
    let embedder = Fixed {
        vector: vec![0.0, 1.0, 0.0, 0.0],
    };
    let book_library = open_library(&book_paths, embedder.clone()).await;
    let paper_library = open_library(&paper_paths, embedder)
        .await
        .with_kind(ItemKind::Paper);
    let ops = attach_papers(
        ops_over(&tmp, &book_paths, book_library),
        &tmp,
        &paper_paths,
        paper_library,
    );

    let citations = search_unified(&ops, "q", SearchOptions::default(), Some(2))
        .await
        .expect("unified search");
    assert_eq!(citations.len(), 2, "four candidates cut to top_k");
    assert_eq!(citations[0].kind, ItemKind::Book);
    assert_eq!(citations[0].text, "book near");
    assert_eq!(citations[1].kind, ItemKind::Paper);
    assert_eq!(citations[1].text, "paper near");
    assert!(
        citations[0].distance < citations[1].distance,
        "merged order must ascend by distance: {} vs {}",
        citations[0].distance,
        citations[1].distance
    );
}

#[tokio::test]
async fn a_reranker_stage_reorders_and_caps_the_results() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = seed_store(
        tmp.path(),
        "books",
        &[
            ("alpha", vec![0.0, 1.0, 0.0, 0.0]),
            ("beta", vec![0.1, 0.9, 0.0, 0.0]),
            ("gamma", vec![0.2, 0.8, 0.0, 0.0]),
        ],
    )
    .await;
    let embedder = Fixed {
        vector: vec![0.0, 1.0, 0.0, 0.0],
    };
    let library = open_library(&paths, embedder).await;
    // The mock scores the ANN-worst candidate highest, proving the
    // stage's order — not the recall order — reaches the caller.
    let url = rerank_mock(
        r#"{"results":[{"index":2,"relevance_score":0.9},
                       {"index":0,"relevance_score":0.4},
                       {"index":1,"relevance_score":0.2}]}"#,
    );
    let ops = ops_over(&tmp, &paths, library).with_reranker(stage_at(&url, 10, 2));

    // keep = min(final_k = default 5, top_k_out = 2): the stage width
    // caps the caller's ask.
    let hits = search(&ops, "q", SearchOptions::default(), None)
        .await
        .expect("reranked search");
    assert_eq!(hits.len(), 2, "top_k_out caps the stage output");
    assert_eq!(hits[0].text, "gamma");
    assert_eq!(hits[0].rerank_score, Some(0.9));
    assert_eq!(hits[1].text, "alpha");
    assert_eq!(hits[1].rerank_score, Some(0.4));

    // keep = min(final_k = 1, top_k_out = 2): the caller's ask caps
    // the stage width.
    let hits = search(&ops, "q", SearchOptions::default(), Some(1))
        .await
        .expect("reranked search with top_k=1");
    assert_eq!(hits.len(), 1, "final_k caps the stage output");
    assert_eq!(hits[0].text, "gamma");
}

#[tokio::test]
async fn a_failed_rerank_stage_fails_the_search() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = seed_store(
        tmp.path(),
        "books",
        &[("the one passage", vec![1.0, 0.0, 0.0, 0.0])],
    )
    .await;
    let library = open_library(&paths, Fixed::unit_x()).await;
    let ops = ops_over(&tmp, &paths, library).with_reranker(stage_at(&dead_address(), 10, 2));

    // Candidates exist, the stage is unreachable: the search fails
    // rather than silently returning the unreranked order.
    let err = search(&ops, "q", SearchOptions::default(), None)
        .await
        .expect_err("stage failure fails the search");
    assert!(matches!(err, OpsError::Rerank(_)), "{err:?}");
}

#[tokio::test]
async fn an_empty_recall_skips_the_rerank_stage() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = seed_store(tmp.path(), "books", &[]).await;
    let library = open_library(&paths, Fixed::unit_x()).await;
    // The dead client proves the shortcut: with nothing recalled, the
    // stage is never called and the search still succeeds.
    let ops = ops_over(&tmp, &paths, library).with_reranker(stage_at(&dead_address(), 10, 2));

    let hits = search(&ops, "q", SearchOptions::default(), None)
        .await
        .expect("empty recall bypasses the stage");
    assert!(hits.is_empty());
}
