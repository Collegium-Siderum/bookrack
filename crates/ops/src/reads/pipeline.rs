// SPDX-License-Identifier: Apache-2.0

//! Read ops over the book-level pipeline audit trail.

use bookrack_catalog::Catalog;
use bookrack_core::PartitionIdx;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::audit::PipelineAuditEntry;
use crate::recorder::record_call_sync;

/// Read the book-level pipeline audit trail for one book, oldest first.
///
/// `book_pipeline_audit` rows outlive their book by design: `bookrack
/// remove` drops the `intake` row but preserves the audit history.
/// This read therefore surfaces rows whenever any exist, regardless of
/// whether the `intake_id` is still registered. Only when no rows
/// exist AND no `intake` is registered for the id is it reported as
/// [`OpsError::IntakeNotFound`] — that is the "ghost id" case.
pub fn show_pipeline_trail<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
) -> Result<Vec<PipelineAuditEntry>> {
    record_call_sync!(
        ops,
        "library.show_pipeline_trail",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let catalog = Catalog::open_read_only(ops.catalog_db())?;
            let book_root_id = PartitionIdx::new(intake_id).root().get();
            let rows = catalog.pipeline_audit_for_book(book_root_id)?;
            if rows.is_empty() && catalog.intake_by_id(intake_id)?.is_none() {
                return Err(OpsError::IntakeNotFound { intake_id });
            }
            Ok(rows.into_iter().map(PipelineAuditEntry::from_row).collect())
        }
    )
}
