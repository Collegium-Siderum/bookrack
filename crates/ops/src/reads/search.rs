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
