// SPDX-License-Identifier: Apache-2.0

//! Dense retrieval ops.
//!
//! Search requires a warm [`bookrack_query::Library`]. An [`Ops`] built
//! with [`Ops::catalog_only`](crate::Ops::catalog_only) returns
//! [`OpsError::SearchUnavailable`] from every function here. The
//! existence-of-intake check goes through the catalog open directly, so
//! a missing intake is reported as [`OpsError::IntakeNotFound`] without
//! a vector roundtrip.

use std::path::Path;

use bookrack_catalog::{Catalog, NewRetrievalCall};
use bookrack_embed::Embedder;
use bookrack_query::{Citation, Library, SearchOptions};

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::recorder::{Recorder, record_call_async};

/// Search the library and return cited passages, nearest first.
///
/// `overrides` layers per-call overrides on top of the persisted meta
/// defaults — see [`bookrack_search::retrieve_with`] for the merge order.
/// Pass [`SearchOptions::default()`] to use the meta defaults unchanged.
pub async fn search<E: Embedder>(
    ops: &Ops<E>,
    query: &str,
    overrides: SearchOptions,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    let recorder = Recorder::start(
        ops,
        "library.search",
        serde_json::json!({
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
    );
    let result = async {
        let library = ops.library().ok_or(OpsError::SearchUnavailable)?;
        Ok(library.search_with(query, overrides, top_k).await?)
    }
    .await;
    let retrieval = book_retrieval_payload(ops, query, top_k, &result);
    recorder.finish_with_retrieval(&result, retrieval);
    result
}

/// Search inside one book's partition.
///
/// `overrides` layers per-call overrides on top of the persisted meta
/// defaults — see [`bookrack_search::retrieve_with_partition`] for the
/// merge order. Returns [`OpsError::IntakeNotFound`] when no such intake
/// is registered, [`OpsError::SearchUnavailable`] when this [`Ops`] is
/// catalog-only.
pub async fn search_in_book<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
    query: &str,
    overrides: SearchOptions,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    let recorder = Recorder::start(
        ops,
        "library.search_in_book",
        serde_json::json!({
            "intake_id": intake_id,
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
    );
    let result = async {
        let library = ops.library().ok_or(OpsError::SearchUnavailable)?;
        let catalog = Catalog::open_read_only(ops.catalog_db())?;
        if catalog.intake_by_id(intake_id)?.is_none() {
            return Err(OpsError::IntakeNotFound { intake_id });
        }
        Ok(library
            .search_in_book_with(intake_id, query, overrides, top_k)
            .await?)
    }
    .await;
    let retrieval = book_retrieval_payload(ops, query, top_k, &result);
    recorder.finish_with_retrieval(&result, retrieval);
    result
}

/// Search the paper-side store and return cited passages.
///
/// Mirrors [`search`] for the paper pipeline. Returns
/// [`OpsError::PapersBackendNotConfigured`] when this [`Ops`] has no
/// papers backend.
pub async fn search_paper<E: Embedder>(
    ops: &Ops<E>,
    query: &str,
    overrides: SearchOptions,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    let recorder = Recorder::start(
        ops,
        "library.search_paper",
        serde_json::json!({
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
    );
    let result = async {
        let papers_library = ops
            .papers_library()
            .ok_or(OpsError::PapersBackendNotConfigured)?;
        Ok(papers_library.search_with(query, overrides, top_k).await?)
    }
    .await;
    let retrieval = paper_retrieval_payload(ops, query, top_k, &result);
    recorder.finish_with_retrieval(&result, retrieval);
    result
}

/// Search inside one paper's partition on the paper-side store.
///
/// Mirrors [`search_in_book`] for the paper pipeline. Returns
/// [`OpsError::PapersBackendNotConfigured`] when this [`Ops`] has no
/// papers backend, or [`OpsError::IntakeNotFound`] when no such
/// intake exists on the paper catalog.
pub async fn search_in_paper<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
    query: &str,
    overrides: SearchOptions,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    let recorder = Recorder::start(
        ops,
        "library.search_in_paper",
        serde_json::json!({
            "intake_id": intake_id,
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
    );
    let result = async {
        let papers_library = ops
            .papers_library()
            .ok_or(OpsError::PapersBackendNotConfigured)?;
        let papers_catalog_db = ops
            .papers_catalog_db()
            .ok_or(OpsError::PapersBackendNotConfigured)?;
        let catalog = Catalog::open_read_only(papers_catalog_db)?;
        if catalog.intake_by_id(intake_id)?.is_none() {
            return Err(OpsError::IntakeNotFound { intake_id });
        }
        Ok(papers_library
            .search_in_paper_with(intake_id, query, overrides, top_k)
            .await?)
    }
    .await;
    let retrieval = paper_retrieval_payload(ops, query, top_k, &result);
    recorder.finish_with_retrieval(&result, retrieval);
    result
}

/// Search both the book-side and paper-side stores and merge the
/// nearest-first results. The result list carries each hit's
/// originating pipeline through `Citation.kind`.
///
/// Returns [`OpsError::SearchUnavailable`] when the book-side library
/// is absent and [`OpsError::PapersBackendNotConfigured`] when the
/// paper-side is absent.
///
/// No retrieval sidecar is recorded here: the merged result spans two
/// stores with two distinct corpus fingerprints, which the
/// single-fingerprint `retrieval_calls` row cannot represent. The
/// per-store ops remain the recorded surface.
pub async fn search_unified<E: Embedder>(
    ops: &Ops<E>,
    query: &str,
    overrides: SearchOptions,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    record_call_async!(
        ops,
        "library.search.unified",
        serde_json::json!({
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
        {
            let books = ops.library().ok_or(OpsError::SearchUnavailable)?;
            let papers = ops
                .papers_library()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let effective_k = top_k.unwrap_or_else(|| books.default_top_k());
            let mut combined = books
                .search_with(query, overrides.clone(), Some(effective_k))
                .await?;
            let paper_hits = papers
                .search_with(query, overrides, Some(effective_k))
                .await?;
            combined.extend(paper_hits);
            combined.sort_by(|a, b| {
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            combined.truncate(effective_k);
            Ok(combined)
        }
    )
}

/// Compose the corpus fingerprint of the store rooted at `corpus_db`
/// and `lancedb_dir`: the four `index_meta` build stamps joined with
/// the ANN kind from `vectors_meta.json`. A store with no meta file
/// scans brute-force, so that kind stands in as the fifth stamp.
fn corpus_fingerprint_at(corpus_db: &Path, lancedb_dir: &Path) -> Result<String> {
    let corpus = bookrack_corpus::Corpus::open_read_only(corpus_db)?;
    let ann_kind = bookrack_vectors::meta::load(lancedb_dir)?
        .map(|meta| meta.kind)
        .unwrap_or_else(|| bookrack_vectors::AnnKind::BruteForce.as_str().to_string());
    Ok(corpus.compose_corpus_fingerprint(&ann_kind)?)
}

/// Build the retrieval sidecar payload for one settled single-store
/// search: the store's corpus fingerprint, the effective depth, the
/// query, and one `(norm_chunk_sha256, distance)` pair per returned
/// citation. Returns `None` when the fingerprint cannot be composed
/// (e.g. an unstamped index) — recording is opportunistic and must
/// never fail the search itself.
fn retrieval_payload<E: Embedder>(
    library: &Library<E>,
    corpus_db: &Path,
    lancedb_dir: &Path,
    query: &str,
    top_k: Option<usize>,
    citations: &[Citation],
) -> Option<NewRetrievalCall> {
    let fingerprint = match corpus_fingerprint_at(corpus_db, lancedb_dir) {
        Ok(fingerprint) => fingerprint,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not compose the corpus fingerprint; skipping the retrieval sidecar",
            );
            return None;
        }
    };
    Some(NewRetrievalCall {
        fingerprint,
        top_k: top_k.unwrap_or_else(|| library.default_top_k()) as i64,
        query_text: Some(query.to_string()),
        hits: citations
            .iter()
            .map(|citation| (citation.norm_chunk_sha256.clone(), citation.distance))
            .collect(),
    })
}

/// The retrieval payload of one settled book-side search, or `None`
/// when the result failed or the library is absent.
fn book_retrieval_payload<E: Embedder>(
    ops: &Ops<E>,
    query: &str,
    top_k: Option<usize>,
    result: &Result<Vec<Citation>>,
) -> Option<NewRetrievalCall> {
    match (result, ops.library()) {
        (Ok(citations), Some(library)) => retrieval_payload(
            library,
            ops.corpus_db(),
            ops.lancedb_dir(),
            query,
            top_k,
            citations,
        ),
        _ => None,
    }
}

/// The retrieval payload of one settled paper-side search, or `None`
/// when the result failed or no papers backend is attached.
fn paper_retrieval_payload<E: Embedder>(
    ops: &Ops<E>,
    query: &str,
    top_k: Option<usize>,
    result: &Result<Vec<Citation>>,
) -> Option<NewRetrievalCall> {
    match (
        result,
        ops.papers_library(),
        ops.papers_corpus_db(),
        ops.papers_lancedb_dir(),
    ) {
        (Ok(citations), Some(library), Some(corpus_db), Some(lancedb_dir)) => {
            retrieval_payload(library, corpus_db, lancedb_dir, query, top_k, citations)
        }
        _ => None,
    }
}

/// Render the override knobs onto the recorder row. Skips fields that
/// carry their default so the audit shows only what the caller actually
/// overrode.
fn overrides_to_json(o: &SearchOptions) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if o.bypass_index {
        map.insert("bypass_index".to_string(), serde_json::Value::Bool(true));
    }
    if let Some(n) = o.nprobes {
        map.insert("nprobes".to_string(), serde_json::json!(n));
    }
    if let Some(r) = o.refine_factor {
        map.insert("refine_factor".to_string(), serde_json::json!(r));
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_corpus::{Corpus, IndexStamps};
    use bookrack_vectors::VectorsMeta;

    fn stamped_corpus_at(dir: &std::path::Path) -> std::path::PathBuf {
        let corpus_db = dir.join("corpus.db");
        let corpus = Corpus::open(&corpus_db).expect("open corpus");
        corpus
            .reconcile_index_stamps(&IndexStamps {
                embed_model: "qwen3-embedding:0.6b".to_string(),
                vector_dim: 1024,
                chunk_version: 1,
                normalize_version: 1,
            })
            .expect("stamp corpus");
        corpus_db
    }

    #[test]
    fn corpus_fingerprint_defaults_to_brute_force_without_vectors_meta() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus_db = stamped_corpus_at(tmp.path());
        let lancedb_dir = tmp.path().join("lancedb");

        let fingerprint =
            corpus_fingerprint_at(&corpus_db, &lancedb_dir).expect("compose fingerprint");
        let expected = Corpus::open(&corpus_db)
            .expect("reopen corpus")
            .compose_corpus_fingerprint("brute-force")
            .expect("compose expected");
        assert_eq!(fingerprint, expected);
    }

    #[test]
    fn corpus_fingerprint_takes_the_ann_kind_from_vectors_meta() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus_db = stamped_corpus_at(tmp.path());
        let lancedb_dir = tmp.path().join("lancedb");
        std::fs::create_dir_all(&lancedb_dir).expect("mkdir lancedb");
        bookrack_vectors::meta::store(
            &lancedb_dir,
            &VectorsMeta {
                schema_version: bookrack_vectors::SCHEMA_VERSION,
                min_reader_version: None,
                kind: "ivf-pq".to_string(),
                num_partitions: 16,
                num_sub_vectors: Some(16),
                num_bits: Some(8),
                default_nprobes: 8,
                default_refine_factor: None,
                built_at: "2026-07-03T10:00:00Z".to_string(),
                built_at_chunk_count: 100,
                churn_since_rebuild: 0,
                lance_index_name: "chunks_idx".to_string(),
            },
        )
        .expect("store vectors meta");

        let fingerprint =
            corpus_fingerprint_at(&corpus_db, &lancedb_dir).expect("compose fingerprint");
        let expected = Corpus::open(&corpus_db)
            .expect("reopen corpus")
            .compose_corpus_fingerprint("ivf-pq")
            .expect("compose expected");
        assert_eq!(fingerprint, expected);
    }
}
