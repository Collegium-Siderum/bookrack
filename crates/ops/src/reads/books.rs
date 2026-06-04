// SPDX-License-Identifier: Apache-2.0

//! Browse the book catalog: list / find / show / TOC / aggregate stats.
//!
//! Each function proxies straight through to the warm
//! [`bookrack_query::Library`]. The DTOs come back unchanged; the only
//! adapter layer here is promoting "no such intake" to the explicit
//! [`OpsError::IntakeNotFound`] so callers do not have to disambiguate
//! the `Option` from a deeper error.

use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::{BookDetail, BookFilter, LibraryStats, ListBooksResult, Toc};

/// List books in catalog order, paginated.
pub fn list_books<E: Embedder>(ops: &Ops<E>, limit: u32, offset: u32) -> Result<ListBooksResult> {
    Ok(ops.library().list_books(limit, offset)?)
}

/// List books matching `filter`, paginated.
pub fn find_books<E: Embedder>(
    ops: &Ops<E>,
    filter: BookFilter,
    limit: u32,
    offset: u32,
) -> Result<ListBooksResult> {
    Ok(ops.library().find_books(filter, limit, offset)?)
}

/// Fetch the full bibliographic record of one book by intake id.
pub fn show_book<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<BookDetail> {
    match ops.library().show_book(intake_id)? {
        Some(detail) => Ok(detail),
        None => Err(OpsError::IntakeNotFound { intake_id }),
    }
}

/// Project the table of contents of one book.
pub fn show_toc<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<Toc> {
    match ops.library().show_toc(intake_id)? {
        Some(toc) => Ok(toc),
        None => Err(OpsError::IntakeNotFound { intake_id }),
    }
}

/// Aggregate counts across the catalog.
pub fn show_stats<E: Embedder>(ops: &Ops<E>) -> Result<LibraryStats> {
    Ok(ops.library().stats()?)
}
