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
use bookrack_core::PartitionIdx;
use bookrack_embed::Embedder;
use bookrack_query::{Citation, Library, SearchOptions};

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::recorder::{Recorder, record_call_async};

/// An exclusion list that names the wrong side of the library for the
/// requested `kind`. Both control surfaces render it as an
/// invalid-params rejection, so a caller that mixed the two id name
/// spaces up learns it instead of being silently ignored.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "library.search: {field} does not apply to kind={kind:?}; \
     pass it with kind=\"{owning_kind}\" or kind=\"all\", or drop it"
)]
pub struct ExclusionKindMismatch {
    /// The kind the caller asked for.
    pub kind: String,
    /// The exclusion field that does not belong to that kind.
    pub field: String,
    /// The kind the field does belong to.
    pub owning_kind: String,
}

/// Check the two exclusion lists against the requested `kind`: `"book"`
/// admits only book exclusions, `"paper"` only paper exclusions, and
/// `"all"` admits both. Empty lists are always admissible.
///
/// A `kind` outside the three known values is accepted here — this
/// function decides only whether the *fields* apply, and the caller's
/// own dispatch rejects the unknown kind with its own message.
pub fn validate_exclusions(
    kind: &str,
    book_ids: &[i64],
    paper_ids: &[i64],
) -> std::result::Result<(), ExclusionKindMismatch> {
    let offender = match kind {
        "book" if !paper_ids.is_empty() => Some(("exclude_paper_intake_ids", "paper")),
        "paper" if !book_ids.is_empty() => Some(("exclude_book_intake_ids", "book")),
        _ => None,
    };
    match offender {
        None => Ok(()),
        Some((field, owning_kind)) => Err(ExclusionKindMismatch {
            kind: kind.to_string(),
            field: field.to_string(),
            owning_kind: owning_kind.to_string(),
        }),
    }
}

/// Project intake ids onto the id partitions a search excludes. The two
/// are the same number by construction, so this is a rewrap: ids that
/// name no stored book yield partitions no row falls in, which is
/// exactly "nothing was excluded".
pub fn exclusions_to_partitions(ids: &[i64]) -> Vec<PartitionIdx> {
    ids.iter().copied().map(PartitionIdx::new).collect()
}

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
        match ops.rerank_stage() {
            None => Ok(library.search_with(query, overrides, top_k).await?),
            Some(stage) => {
                let candidates = library
                    .search_with(query, overrides, Some(stage.top_k_in))
                    .await?;
                let final_k = top_k.unwrap_or_else(|| library.default_top_k());
                apply_rerank_stage(stage, query, candidates, final_k).await
            }
        }
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
        match ops.rerank_stage() {
            None => Ok(library
                .search_in_book_with(intake_id, query, overrides, top_k)
                .await?),
            Some(stage) => {
                let candidates = library
                    .search_in_book_with(intake_id, query, overrides, Some(stage.top_k_in))
                    .await?;
                let final_k = top_k.unwrap_or_else(|| library.default_top_k());
                apply_rerank_stage(stage, query, candidates, final_k).await
            }
        }
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
        match ops.rerank_stage() {
            None => Ok(papers_library.search_with(query, overrides, top_k).await?),
            Some(stage) => {
                let candidates = papers_library
                    .search_with(query, overrides, Some(stage.top_k_in))
                    .await?;
                let final_k = top_k.unwrap_or_else(|| papers_library.default_top_k());
                apply_rerank_stage(stage, query, candidates, final_k).await
            }
        }
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
        match ops.rerank_stage() {
            None => Ok(papers_library
                .search_in_paper_with(intake_id, query, overrides, top_k)
                .await?),
            Some(stage) => {
                let candidates = papers_library
                    .search_in_paper_with(intake_id, query, overrides, Some(stage.top_k_in))
                    .await?;
                let final_k = top_k.unwrap_or_else(|| papers_library.default_top_k());
                apply_rerank_stage(stage, query, candidates, final_k).await
            }
        }
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
/// Each store gets its own [`SearchOptions`]: the two id name spaces
/// are disjoint, so an exclusion list only means something on the side
/// it was addressed to. The ANN knobs are normally identical on both.
///
/// Returns [`OpsError::SearchUnavailable`] when the book-side library
/// is absent and [`OpsError::PapersBackendNotConfigured`] when the
/// paper-side is absent.
///
/// The two stores are recalled concurrently. When both libraries serve
/// the same embedding model — the shape a daemon builds, since one
/// embed configuration drives both — the query is embedded once and the
/// vector is recalled against each store; libraries reporting different
/// models fall back to embedding per store, still concurrently, because
/// their vectors live in different spaces. The shared embed happens
/// before either store is consulted, so a unified search over two empty
/// stores pays one embed round trip that the per-store ops would have
/// skipped.
///
/// No retrieval sidecar is recorded here: the merged result spans two
/// stores with two distinct corpus fingerprints, which the
/// single-fingerprint `retrieval_calls` row cannot represent. The
/// per-store ops remain the recorded surface.
pub async fn search_unified<E: Embedder>(
    ops: &Ops<E>,
    query: &str,
    book_overrides: SearchOptions,
    paper_overrides: SearchOptions,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    record_call_async!(
        ops,
        "library.search.unified",
        serde_json::json!({
            "query": query,
            "top_k": top_k,
            "overrides": {
                "books": overrides_to_json(&book_overrides),
                "papers": overrides_to_json(&paper_overrides),
            },
        }),
        {
            let books = ops.library().ok_or(OpsError::SearchUnavailable)?;
            let papers = ops
                .papers_library()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let effective_k = top_k.unwrap_or_else(|| books.default_top_k());
            // With a reranker, each side recalls the full candidate
            // window and the distance merge below narrows the union
            // back to `top_k_in`, so the stage scores one profile-sized
            // window regardless of how the candidates split across
            // stores.
            let recall_k = match ops.rerank_stage() {
                Some(stage) => stage.top_k_in,
                None => effective_k,
            };
            let (book_hits, paper_hits) = if books.embed_model() == papers.embed_model() {
                let query_vector = books.embed_query(query).await?;
                tokio::try_join!(
                    books.search_with_vector(&query_vector, book_overrides, Some(recall_k)),
                    papers.search_with_vector(&query_vector, paper_overrides, Some(recall_k)),
                )?
            } else {
                tokio::try_join!(
                    books.search_with(query, book_overrides, Some(recall_k)),
                    papers.search_with(query, paper_overrides, Some(recall_k)),
                )?
            };
            let mut combined = book_hits;
            combined.extend(paper_hits);
            combined.sort_by(|a, b| {
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            match ops.rerank_stage() {
                None => {
                    combined.truncate(effective_k);
                    Ok(combined)
                }
                Some(stage) => {
                    combined.truncate(stage.top_k_in);
                    apply_rerank_stage(stage, query, combined, effective_k).await
                }
            }
        }
    )
}

/// Run the reranker stage over recalled candidates: score every
/// candidate's text against the query, reorder by descending
/// relevance, and keep at most `min(final_k, top_k_out)` — the
/// caller's ask, capped by the profile's stage width. A stage failure
/// fails the search; the profile promises the reranked order as part
/// of an atomic combination, and under the supervised backend an
/// unreachable server is a transient restart window, so a silent
/// fallback would deliver a knowingly worse ranking.
async fn apply_rerank_stage(
    stage: &crate::RerankStage,
    query: &str,
    candidates: Vec<Citation>,
    final_k: usize,
) -> Result<Vec<Citation>> {
    let keep = final_k.min(stage.top_k_out);
    if candidates.is_empty() || keep == 0 {
        return Ok(Vec::new());
    }
    let documents: Vec<String> = candidates.iter().map(|c| c.text.clone()).collect();
    let ranked = stage.client.rerank(query, &documents, keep).await?;
    take_ranked(candidates, &ranked)
}

/// Reorder candidates by the ranking the server returned, stamping
/// each survivor with its relevance score. A ranking that names a
/// candidate twice or out of range is a malformed response — the
/// client already bounds-checks, so the duplicate check here is the
/// remaining guard.
fn take_ranked(
    candidates: Vec<Citation>,
    ranked: &[bookrack_rerank::RankedDocument],
) -> Result<Vec<Citation>> {
    let mut slots: Vec<Option<Citation>> = candidates.into_iter().map(Some).collect();
    ranked
        .iter()
        .map(|entry| {
            let mut citation = slots
                .get_mut(entry.index)
                .and_then(Option::take)
                .ok_or_else(|| {
                    OpsError::Rerank(bookrack_rerank::RerankError::MalformedResponse(format!(
                        "result index {} repeated or out of range",
                        entry.index
                    )))
                })?;
            citation.rerank_score = Some(entry.score);
            Ok(citation)
        })
        .collect()
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
    if !o.exclude_partitions.is_empty() {
        let excluded: Vec<i64> = o.exclude_partitions.iter().map(|p| p.get()).collect();
        map.insert(
            "exclude_partitions".to_string(),
            serde_json::json!(excluded),
        );
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

    fn candidate(text: &str, distance: f32) -> Citation {
        use bookrack_core::{ItemKind, NodeId};
        Citation {
            text: text.to_string(),
            breadcrumb: "Book \u{203a} Chapter".to_string(),
            intake_id: 1,
            kind: ItemKind::Book,
            toc_position: None,
            enclosing_node_id: None,
            start_node_id: NodeId::new(100_000_001),
            start_char_offset: 0,
            end_node_id: NodeId::new(100_000_001),
            end_char_offset: text.len() as i32,
            norm_chunk_sha256: "abc".to_string(),
            distance,
            rerank_score: None,
        }
    }

    #[test]
    fn take_ranked_reorders_and_stamps_scores() {
        use bookrack_rerank::RankedDocument;
        let candidates = vec![
            candidate("a", 0.1),
            candidate("b", 0.2),
            candidate("c", 0.3),
        ];
        let ranked = [
            RankedDocument {
                index: 2,
                score: 0.9,
            },
            RankedDocument {
                index: 0,
                score: 0.4,
            },
        ];
        let reranked = take_ranked(candidates, &ranked).expect("ranking applies");
        assert_eq!(reranked.len(), 2);
        assert_eq!(reranked[0].text, "c");
        assert_eq!(reranked[0].rerank_score, Some(0.9));
        assert_eq!(reranked[1].text, "a");
        assert_eq!(reranked[1].rerank_score, Some(0.4));
        // The ANN distance survives alongside the stage score.
        assert!((reranked[0].distance - 0.3).abs() < 1e-6);
    }

    #[test]
    fn exclusion_validation_covers_every_kind_and_side() {
        // kind="book": book exclusions apply, paper exclusions do not.
        assert!(validate_exclusions("book", &[1, 2], &[]).is_ok());
        let err = validate_exclusions("book", &[], &[7]).expect_err("paper ids under kind=book");
        assert_eq!(err.field, "exclude_paper_intake_ids");
        assert_eq!(err.owning_kind, "paper");
        assert!(
            err.to_string().contains("exclude_paper_intake_ids")
                && err.to_string().contains("book"),
            "the message must name both the field and the kind: {err}"
        );

        // kind="paper": the mirror image.
        assert!(validate_exclusions("paper", &[], &[7]).is_ok());
        let err = validate_exclusions("paper", &[1], &[]).expect_err("book ids under kind=paper");
        assert_eq!(err.field, "exclude_book_intake_ids");
        assert_eq!(err.owning_kind, "book");

        // kind="all": both sides apply at once.
        assert!(validate_exclusions("all", &[1], &[7]).is_ok());
        assert!(validate_exclusions("all", &[], &[]).is_ok());

        // An unknown kind is not this function's business; the caller's
        // dispatch rejects it with its own message.
        assert!(validate_exclusions("sideways", &[1], &[7]).is_ok());
    }

    #[test]
    fn intake_ids_project_onto_the_partitions_of_the_same_number() {
        use bookrack_core::PartitionIdx;
        assert!(exclusions_to_partitions(&[]).is_empty());
        assert_eq!(
            exclusions_to_partitions(&[3, 1, 4]),
            vec![
                PartitionIdx::new(3),
                PartitionIdx::new(1),
                PartitionIdx::new(4)
            ],
            "order is preserved and the id is the partition index",
        );
        // Ids that name no stored book are projected unchecked; they
        // simply exclude a range no row falls in.
        assert_eq!(
            exclusions_to_partitions(&[0, -5]),
            vec![PartitionIdx::new(0), PartitionIdx::new(-5)]
        );
    }

    #[test]
    fn overrides_render_only_what_the_caller_set() {
        // Nothing overridden: an empty object, not a object full of
        // nulls and defaults.
        let empty = overrides_to_json(&SearchOptions::default());
        assert_eq!(empty, serde_json::json!({}));

        let full = overrides_to_json(&SearchOptions {
            bypass_index: true,
            nprobes: Some(32),
            refine_factor: Some(2),
            exclude_partitions: exclusions_to_partitions(&[4, 9]),
        });
        assert_eq!(
            full,
            serde_json::json!({
                "bypass_index": true,
                "nprobes": 32,
                "refine_factor": 2,
                "exclude_partitions": [4, 9],
            })
        );

        // An empty exclusion list omits the key, like the other knobs
        // at their default.
        let no_exclusions = overrides_to_json(&SearchOptions {
            nprobes: Some(8),
            ..SearchOptions::default()
        });
        assert_eq!(no_exclusions, serde_json::json!({"nprobes": 8}));
    }

    #[test]
    fn take_ranked_rejects_a_repeated_index() {
        use bookrack_rerank::RankedDocument;
        let candidates = vec![candidate("a", 0.1), candidate("b", 0.2)];
        let ranked = [
            RankedDocument {
                index: 1,
                score: 0.9,
            },
            RankedDocument {
                index: 1,
                score: 0.8,
            },
        ];
        let err = take_ranked(candidates, &ranked).unwrap_err();
        assert!(matches!(err, OpsError::Rerank(_)), "got {err:?}");
    }
}
