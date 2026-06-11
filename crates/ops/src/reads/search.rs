// SPDX-License-Identifier: Apache-2.0

//! Dense retrieval ops.
//!
//! Search requires a warm [`bookrack_query::Library`]. An [`Ops`] built
//! with [`Ops::catalog_only`](crate::Ops::catalog_only) returns
//! [`OpsError::SearchUnavailable`] from every function here. The
//! existence-of-intake check goes through the catalog open directly, so
//! a missing intake is reported as [`OpsError::IntakeNotFound`] without
//! a vector roundtrip.

use bookrack_catalog::Catalog;
use bookrack_embed::Embedder;
use bookrack_query::{Citation, SearchOptions};

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::recorder::record_call_async;

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
    record_call_async!(
        ops,
        "library.search",
        serde_json::json!({
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
        {
            let library = ops.library().ok_or(OpsError::SearchUnavailable)?;
            Ok(library.search_with(query, overrides, top_k).await?)
        }
    )
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
    record_call_async!(
        ops,
        "library.search_in_book",
        serde_json::json!({
            "intake_id": intake_id,
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
        {
            let library = ops.library().ok_or(OpsError::SearchUnavailable)?;
            let catalog = Catalog::open_read_only(ops.catalog_db())?;
            if catalog.intake_by_id(intake_id)?.is_none() {
                return Err(OpsError::IntakeNotFound { intake_id });
            }
            Ok(library
                .search_in_book_with(intake_id, query, overrides, top_k)
                .await?)
        }
    )
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
    record_call_async!(
        ops,
        "library.search_paper",
        serde_json::json!({
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
        {
            let papers_library = ops
                .papers_library()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            Ok(papers_library.search_with(query, overrides, top_k).await?)
        }
    )
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
    record_call_async!(
        ops,
        "library.search_in_paper",
        serde_json::json!({
            "intake_id": intake_id,
            "query": query,
            "top_k": top_k,
            "overrides": overrides_to_json(&overrides),
        }),
        {
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
    )
}

/// Search both the book-side and paper-side stores and merge the
/// nearest-first results. The result list carries each hit's
/// originating pipeline through `Citation.kind`.
///
/// Returns [`OpsError::SearchUnavailable`] when the book-side library
/// is absent and [`OpsError::PapersBackendNotConfigured`] when the
/// paper-side is absent.
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
