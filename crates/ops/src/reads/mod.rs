// SPDX-License-Identifier: Apache-2.0

//! Read ops over the bookrack library.
//!
//! Each function takes `&Ops<E>` and returns a DTO. Phase A wires the
//! search facade and the seven read methods on
//! [`bookrack_query::Library`]. Later phases add the metadata-audit,
//! pipeline-trail, and library-info reads.

use bookrack_catalog::Catalog;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;

pub mod books;
pub mod info;
pub mod metadata;
pub mod papers;
pub mod papers_metadata;
pub mod passages;
pub mod pipeline;
pub mod search;
pub mod vectors;

/// Open the paper catalog attached to `ops` read-only, if there is one
/// to open.
///
/// Three outcomes: [`OpsError::PapersBackendNotConfigured`] when `ops`
/// carries no papers backend at all; `Ok(None)` when it does but the
/// catalog file has not been created yet — the first glean materialises
/// it, so a library that only ever ingested books has none; `Ok(Some)`
/// otherwise. Each paper read decides what "no catalog yet" means for
/// its own shape: an empty page, zero counts, or an unknown intake.
pub(crate) fn open_papers_catalog<E: Embedder>(ops: &Ops<E>) -> Result<Option<Catalog>> {
    let papers_db = ops
        .papers_catalog_db()
        .ok_or(OpsError::PapersBackendNotConfigured)?;
    Ok(Catalog::try_open_read_only(papers_db)?)
}
