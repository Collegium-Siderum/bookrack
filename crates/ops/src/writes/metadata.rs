// SPDX-License-Identifier: Apache-2.0

//! Metadata write ops: override edits and review-status transitions.
//!
//! Every op here opens the catalog read-write, applies its change, and
//! appends one [`bookrack_catalog::MetadataAudit`] row stamped with the
//! [`crate::Caller`] the [`Ops`] was built with — so a CLI edit and an
//! MCP edit are distinguishable by `actor_kind` / `actor_detail` in the
//! audit trail.

use bookrack_catalog::{
    BOOK_SCOPE, Catalog, NewMetadataAudit, NewOverride, NewReview, STATUS_ACKNOWLEDGED,
    STATUS_APPROVED, STATUS_REJECTED,
};
use bookrack_core::PartitionIdx;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::writes::{
    AcknowledgeMetadataGapRequest, ApproveMetadataRequest, ClearMetadataFieldRequest,
    RejectMetadataRequest, SetMetadataFieldRequest, WriteOutcome,
};

/// Set an override on one bibliographic field of the book root, writing
/// the audit row that records the change.
pub fn set_metadata_field<E: Embedder>(
    ops: &Ops<E>,
    req: SetMetadataFieldRequest,
) -> Result<WriteOutcome> {
    let catalog = Catalog::open(ops.catalog_db())?;
    require_intake(&catalog, req.intake_id)?;

    let effective = catalog.effective_publication_attrs(req.intake_id, BOOK_SCOPE)?;
    let old_value = effective.get(&req.field).map(str::to_string);

    let caller = ops.caller();
    let curated_by = caller.actor_kind.as_str();

    catalog.set_override(&NewOverride::new(
        req.intake_id,
        BOOK_SCOPE,
        req.field.clone(),
        Some(req.value.clone()),
        curated_by,
    ))?;

    let audit = build_audit(
        ops,
        "node_publication_attrs",
        "update",
        Some(req.intake_id),
        Some(req.field.clone()),
        old_value,
        Some(req.value.clone()),
        None,
    );
    let audit_id = catalog.record_metadata_audit(&audit)?;

    Ok(write_outcome(ops, audit_id, true))
}

/// Remove an override on one field, reverting to the extracted value.
pub fn clear_metadata_field<E: Embedder>(
    ops: &Ops<E>,
    req: ClearMetadataFieldRequest,
) -> Result<WriteOutcome> {
    let catalog = Catalog::open(ops.catalog_db())?;
    require_intake(&catalog, req.intake_id)?;

    let effective = catalog.effective_publication_attrs(req.intake_id, BOOK_SCOPE)?;
    let old_value = effective.get(&req.field).map(str::to_string);

    let existed = catalog.clear_override(req.intake_id, BOOK_SCOPE, &req.field)?;

    // Audit either way: the trail records that someone tried.
    let audit = build_audit(
        ops,
        "node_publication_attrs",
        "delete",
        Some(req.intake_id),
        Some(req.field),
        if existed { old_value } else { None },
        None,
        None,
    );
    let audit_id = catalog.record_metadata_audit(&audit)?;

    Ok(write_outcome(ops, audit_id, existed))
}

/// Acknowledge a metadata gap: leaves the audit verdict alone but flips
/// the review row to `acknowledged` with a recorded reason.
pub fn acknowledge_metadata_gap<E: Embedder>(
    ops: &Ops<E>,
    req: AcknowledgeMetadataGapRequest,
) -> Result<WriteOutcome> {
    let catalog = Catalog::open(ops.catalog_db())?;
    require_intake(&catalog, req.intake_id)?;

    let audit = build_audit(
        ops,
        "node_reviews",
        "acknowledge_gate",
        Some(req.intake_id),
        None,
        None,
        None,
        Some(req.reason.clone()),
    );
    let audit_id = catalog.record_metadata_audit(&audit)?;

    let caller = ops.caller();
    catalog.upsert_review(&NewReview::new(
        req.intake_id,
        BOOK_SCOPE,
        caller.actor_kind.as_str(),
        STATUS_ACKNOWLEDGED,
    ))?;

    Ok(write_outcome(ops, audit_id, true))
}

/// Approve the record. The audit verdict is unchanged; the review row
/// is flipped to `approved`.
pub fn approve_metadata<E: Embedder>(
    ops: &Ops<E>,
    req: ApproveMetadataRequest,
) -> Result<WriteOutcome> {
    let catalog = Catalog::open(ops.catalog_db())?;
    require_intake(&catalog, req.intake_id)?;

    let audit = build_audit(
        ops,
        "node_reviews",
        "approve",
        Some(req.intake_id),
        None,
        None,
        None,
        req.reason.clone(),
    );
    let audit_id = catalog.record_metadata_audit(&audit)?;

    let caller = ops.caller();
    let mut review = NewReview::new(
        req.intake_id,
        BOOK_SCOPE,
        caller.actor_kind.as_str(),
        STATUS_APPROVED,
    );
    if let Some(r) = req.reason {
        review = review.notes(r);
    }
    catalog.upsert_review(&review)?;

    Ok(write_outcome(ops, audit_id, true))
}

/// Reject the book. Pipeline rows stay in place so downstream consumers
/// can filter on `rejected`.
pub fn reject_metadata<E: Embedder>(
    ops: &Ops<E>,
    req: RejectMetadataRequest,
) -> Result<WriteOutcome> {
    let catalog = Catalog::open(ops.catalog_db())?;
    require_intake(&catalog, req.intake_id)?;

    let audit = build_audit(
        ops,
        "node_reviews",
        "reject",
        Some(req.intake_id),
        None,
        None,
        None,
        Some(req.reason.clone()),
    );
    let audit_id = catalog.record_metadata_audit(&audit)?;

    let caller = ops.caller();
    catalog.upsert_review(
        &NewReview::new(
            req.intake_id,
            BOOK_SCOPE,
            caller.actor_kind.as_str(),
            STATUS_REJECTED,
        )
        .notes(req.reason),
    )?;

    Ok(write_outcome(ops, audit_id, true))
}

fn require_intake(catalog: &Catalog, intake_id: i64) -> Result<()> {
    if catalog.intake_by_id(intake_id)?.is_none() {
        return Err(OpsError::IntakeNotFound { intake_id });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // Mirrors the columns of NewMetadataAudit; collapsing into a builder would just hide the same field list.
fn build_audit<E: Embedder>(
    ops: &Ops<E>,
    table_name: &str,
    action: &str,
    intake_id: Option<i64>,
    field: Option<String>,
    old_value: Option<String>,
    new_value: Option<String>,
    reason: Option<String>,
) -> NewMetadataAudit {
    let caller = ops.caller();
    let mut audit = NewMetadataAudit::new(table_name, action, caller.actor_kind);
    audit.node_id = intake_id.map(|id| PartitionIdx::new(id).root().get());
    audit.field = field;
    audit.old_value = old_value;
    audit.new_value = new_value;
    audit.actor_detail = caller.actor_detail.clone();
    audit.session_id = caller.session_id.clone();
    audit.reason = reason.or_else(|| caller.reason.clone());
    audit
}

fn write_outcome<E: Embedder>(ops: &Ops<E>, audit_id: i64, changed: bool) -> WriteOutcome {
    let caller = ops.caller();
    WriteOutcome {
        audit_id,
        actor_kind: caller.actor_kind.as_str().to_string(),
        actor_detail: caller.actor_detail.clone(),
        changed,
    }
}
