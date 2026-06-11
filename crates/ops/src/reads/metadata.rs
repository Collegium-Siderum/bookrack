// SPDX-License-Identifier: Apache-2.0

//! Read ops over the metadata audit trail and the review queue.

use bookrack_catalog::{Catalog, IntakeFilter, STATUS_ACKNOWLEDGED, STATUS_PENDING};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::audit::AuditTrailEntry;
use crate::dto::metadata_report::{
    MetadataAuditReport, MetadataListPage, MetadataListRow, MetadataReport,
};
use crate::dto::{BookDetail, clamp_limit};
use crate::recorder::record_call_sync;

/// Read the metadata-status record for one book: bibliographic detail
/// plus the persisted audit verdict, confidence, and review status.
pub fn show_metadata_audit<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<MetadataReport> {
    record_call_sync!(
        ops,
        "library.show_metadata_audit",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let catalog = Catalog::open_read_only(ops.catalog_db())?;
            let Some(intake) = catalog.intake_by_id(intake_id)? else {
                return Err(OpsError::IntakeNotFound { intake_id });
            };
            let effective =
                catalog.effective_publication_attrs(intake.intake_id, ItemKind::Book)?;
            let overrides = catalog.overrides_for_address(intake.intake_id, ItemKind::Book)?;
            let contributors =
                catalog.contributors_for_address(intake.intake_id, ItemKind::Book)?;
            let attrs = catalog.publication_attrs(intake.intake_id, ItemKind::Book)?;
            let review_status = catalog
                .review(intake.intake_id, ItemKind::Book)?
                .map(|r| r.status);
            let stored_verdict = attrs.as_ref().and_then(|a| a.audit_verdict.clone());
            let stored_confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
            let book = BookDetail::build(intake, effective, overrides, contributors);
            Ok(MetadataReport {
                intake_id,
                book,
                stored_verdict,
                stored_confidence,
                review_status,
            })
        }
    )
}

/// Recompute the metadata plausibility audit for one book from its
/// cached extraction envelope and return the full per-field report —
/// grades, flags, and hints plus the TOC shape flags — next to the
/// stored rollup for comparison. Pure read: nothing is written back;
/// `metadata.reaudit` is the write path that refreshes the rollup.
pub fn show_metadata_report<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
    audit_data: &bookrack_ingest::AuditData,
    audit_profile: &bookrack_ingest::AuditProfile,
) -> Result<MetadataAuditReport> {
    record_call_sync!(
        ops,
        "library.show_metadata_report",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let catalog = Catalog::open_read_only(ops.catalog_db())?;
            let report = bookrack_ingest::reaudit::build_report(
                &catalog,
                intake_id,
                audit_data,
                audit_profile,
            )
            .map_err(|e| match e {
                bookrack_ingest::IngestError::UnknownIntake(intake_id) => {
                    OpsError::IntakeNotFound { intake_id }
                }
                other => OpsError::Other(anyhow::Error::new(other)),
            })?;
            let overrides = catalog.overrides_for_address(intake_id, ItemKind::Book)?;
            let attrs = catalog.publication_attrs(intake_id, ItemKind::Book)?;
            let review_status = catalog.review(intake_id, ItemKind::Book)?.map(|r| r.status);
            Ok(MetadataAuditReport::build(
                intake_id,
                &audit_profile.name,
                &report,
                &overrides,
                attrs.as_ref().and_then(|a| a.audit_verdict.clone()),
                attrs.as_ref().and_then(|a| a.confidence.clone()),
                review_status,
            ))
        }
    )
}

/// List every registered book with its current confidence and review
/// status. Paginated; no audit / review filtering.
pub fn list_metadata<E: Embedder>(
    ops: &Ops<E>,
    limit: u32,
    offset: u32,
) -> Result<MetadataListPage> {
    record_call_sync!(
        ops,
        "library.list_metadata",
        serde_json::json!({ "limit": limit, "offset": offset }),
        { list_metadata_inner(ops, IntakeFilter::default(), limit, offset) }
    )
}

/// List books still on the review queue: low / medium confidence plus
/// pending / acknowledged review status. Paginated.
pub fn list_pending_reviews<E: Embedder>(
    ops: &Ops<E>,
    limit: u32,
    offset: u32,
) -> Result<MetadataListPage> {
    record_call_sync!(
        ops,
        "library.list_pending_reviews",
        serde_json::json!({ "limit": limit, "offset": offset }),
        {
            let needs_review_confidence: &[&str] = &["low", "medium"];
            let needs_review_status: &[&str] = &[STATUS_PENDING, STATUS_ACKNOWLEDGED];
            let filter = IntakeFilter {
                confidence_in: needs_review_confidence,
                review_status_in: needs_review_status,
                ..IntakeFilter::default()
            };
            list_metadata_inner(ops, filter, limit, offset)
        }
    )
}

/// Shared body of the two paginated metadata listings. Pulled out so
/// the public entry points stay thin and the filter shape is the only
/// thing that differs between them.
fn list_metadata_inner<E: Embedder>(
    ops: &Ops<E>,
    filter: IntakeFilter<'_>,
    limit: u32,
    offset: u32,
) -> Result<MetadataListPage> {
    let (effective_limit, clamp_triggered) = clamp_limit(limit);
    let catalog = Catalog::open_read_only(ops.catalog_db())?;
    let intakes = catalog.find_intakes(&filter, effective_limit, offset)?;
    let total = catalog.count_find_intakes(&filter)?;
    let mut rows = Vec::with_capacity(intakes.len());
    for intake in intakes {
        let effective = catalog.effective_publication_attrs(intake.intake_id, ItemKind::Book)?;
        let title = effective.get("title").map(str::to_string);
        let attrs = catalog.publication_attrs(intake.intake_id, ItemKind::Book)?;
        let confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
        let review_status = catalog
            .review(intake.intake_id, ItemKind::Book)?
            .map(|r| r.status);
        rows.push(MetadataListRow {
            intake_id: intake.intake_id,
            title,
            confidence,
            review_status,
        });
    }
    let returned = rows.len() as u64;
    let truncated = clamp_triggered || u64::from(offset) + returned < total;
    Ok(MetadataListPage {
        rows,
        total,
        truncated,
    })
}

/// Read the metadata-edit audit trail for one book, oldest first.
///
/// `metadata_audit` rows outlive their book by design: `bookrack
/// remove` drops the `intake` row but preserves the audit history.
/// This read therefore surfaces rows whenever any exist, regardless of
/// whether the `intake_id` is still registered. Only when no rows
/// exist AND no `intake` is registered for the id is it reported as
/// [`OpsError::IntakeNotFound`] — that is the "ghost id" case.
pub fn show_audit_trail<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<Vec<AuditTrailEntry>> {
    record_call_sync!(
        ops,
        "library.show_audit_trail",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let catalog = Catalog::open_read_only(ops.catalog_db())?;
            let node_id = PartitionIdx::new(intake_id).root().get();
            let rows = catalog.metadata_audit_for_node(node_id)?;
            if rows.is_empty() && catalog.intake_by_id(intake_id)?.is_none() {
                return Err(OpsError::IntakeNotFound { intake_id });
            }
            Ok(rows.into_iter().map(AuditTrailEntry::from_row).collect())
        }
    )
}
