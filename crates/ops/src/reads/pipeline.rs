// SPDX-License-Identifier: Apache-2.0

//! Read ops over the book-level pipeline audit trail.

use bookrack_catalog::Catalog;
use bookrack_core::PartitionIdx;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::audit::PipelineAuditEntry;

/// Read the book-level pipeline audit trail for one book, oldest first.
pub fn show_pipeline_trail<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
) -> Result<Vec<PipelineAuditEntry>> {
    let catalog = Catalog::open_read_only(ops.catalog_db())?;
    if catalog.intake_by_id(intake_id)?.is_none() {
        return Err(OpsError::IntakeNotFound { intake_id });
    }
    let book_root_id = PartitionIdx::new(intake_id).root().get();
    let rows = catalog.pipeline_audit_for_book(book_root_id)?;
    Ok(rows.into_iter().map(PipelineAuditEntry::from_row).collect())
}
