// SPDX-License-Identifier: Apache-2.0

//! Dense retrieval ops.
//!
//! Phase A proxies to [`bookrack_query::Library`]. An unknown intake on
//! [`search_in_book`] surfaces as [`OpsError::IntakeNotFound`] before
//! the vector roundtrip, so the failure mode matches
//! [`crate::reads::books::show_book`].

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
    Ok(ops.library().search(query, top_k).await?)
}

/// Search inside one book's partition.
pub async fn search_in_book<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
    query: &str,
    top_k: Option<usize>,
) -> Result<Vec<Citation>> {
    if ops.library().show_book(intake_id)?.is_none() {
        return Err(OpsError::IntakeNotFound { intake_id });
    }
    Ok(ops
        .library()
        .search_in_book(intake_id, query, top_k)
        .await?)
}
