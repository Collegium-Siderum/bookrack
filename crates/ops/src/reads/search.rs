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
use bookrack_query::Citation;

use crate::Ops;
use crate::OpsError;
use crate::Result;

/// Search the library and return cited passages, nearest first.
pub async fn search<E: Embedder>(
    ops: &Ops<E>,
    query: &str,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    let library = ops.library().ok_or(OpsError::SearchUnavailable)?;
    Ok(library.search(query, top_k).await?)
}

/// Search inside one book's partition.
///
/// Returns [`OpsError::IntakeNotFound`] when no such intake is
/// registered, [`OpsError::SearchUnavailable`] when this [`Ops`] is
/// catalog-only.
pub async fn search_in_book<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
    query: &str,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    let library = ops.library().ok_or(OpsError::SearchUnavailable)?;
    let catalog = Catalog::open_read_only(ops.catalog_db())?;
    if catalog.intake_by_id(intake_id)?.is_none() {
        return Err(OpsError::IntakeNotFound { intake_id });
    }
    Ok(library.search_in_book(intake_id, query, top_k).await?)
}
