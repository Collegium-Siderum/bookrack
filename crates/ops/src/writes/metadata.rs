// SPDX-License-Identifier: Apache-2.0

//! Metadata write ops: override edits and review-status transitions.
//!
//! Every op here opens the catalog read-write, applies its change, and
//! appends one [`bookrack_catalog::MetadataAudit`] row stamped with the
//! effective [`crate::Caller`] — the task-scope override installed by
//! [`crate::with_caller_override`] when the call arrives through a
//! hosted surface (e.g. MCP), otherwise the caller the [`Ops`] was
//! built with — so a CLI edit and an MCP edit are distinguishable by
//! `actor_kind` / `actor_detail` in the audit trail.

use bookrack_catalog::{
    CONTRIBUTOR_ROLES, Catalog, EDITABLE_FIELDS, NewContributor, NewOverride, STATUS_ACKNOWLEDGED,
    STATUS_APPROVED, STATUS_REJECTED,
};
use bookrack_core::ItemKind;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::writes::{
    AcknowledgeMetadataGapRequest, AddContributorOutcome, AddContributorRequest,
    ApproveMetadataRequest, ClearMetadataFieldRequest, ReauditMetadataRequest, ReauditOutcome,
    RejectMetadataRequest, RemoveContributorRequest, SetMetadataFieldRequest,
    VoidMetadataFieldRequest, WriteOutcome,
};
use crate::recorder::record_call_sync;
use crate::writes::{build_audit, require_intake, write_outcome, write_review_status_inner};

/// Set an override on one bibliographic field of the book root, writing
/// the audit row that records the change. The field must be one of
/// [`EDITABLE_FIELDS`]; an unknown name is rejected before anything is
/// written.
pub fn set_metadata_field<E: Embedder>(
    ops: &Ops<E>,
    req: SetMetadataFieldRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "field": req.field,
        "value": req.value,
        "reason": req.reason,
        "confirmed": req.confirmed,
    });
    record_call_sync!(ops, "library.metadata.set", args, {
        require_editable(&req.field)?;
        let catalog = Catalog::open(ops.catalog_db())?;
        require_intake(&catalog, req.intake_id)?;

        let effective = catalog.effective_publication_attrs(req.intake_id, ItemKind::Book)?;
        let old_value = effective.get(&req.field).map(str::to_string);

        let caller = ops.effective_caller();
        let curated_by = caller.actor_kind.as_str();

        catalog.set_override(
            &NewOverride::new(
                req.intake_id,
                ItemKind::Book,
                req.field.clone(),
                Some(req.value.clone()),
                curated_by,
            )
            .confirmed(req.confirmed),
        )?;

        let audit = build_audit(
            ops,
            "node_publication_attrs",
            "update",
            Some(req.intake_id),
            Some(req.field.clone()),
            old_value,
            Some(req.value.clone()),
            req.reason.clone(),
        );
        let audit_id = catalog.record_metadata_audit(&audit)?;

        Ok(write_outcome(ops, audit_id, true))
    })
}

/// Remove an override on one field, reverting to the extracted value.
///
/// The field name is looser here than on [`set_metadata_field`]: a name
/// outside [`EDITABLE_FIELDS`] is accepted when an override row with
/// that key exists — rows that predate validation must stay removable —
/// and rejected when there is nothing to remove.
pub fn clear_metadata_field<E: Embedder>(
    ops: &Ops<E>,
    req: ClearMetadataFieldRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "field": req.field,
        "reason": req.reason,
    });
    record_call_sync!(ops, "library.metadata.clear", args, {
        let catalog = Catalog::open(ops.catalog_db())?;
        require_intake(&catalog, req.intake_id)?;

        let effective = catalog.effective_publication_attrs(req.intake_id, ItemKind::Book)?;
        let old_value = effective.get(&req.field).map(str::to_string);

        let existed = catalog.clear_override(req.intake_id, ItemKind::Book, &req.field)?;
        if !existed {
            require_editable(&req.field)?;
        }

        // Audit either way: the trail records that someone tried.
        let audit = build_audit(
            ops,
            "node_publication_attrs",
            "delete",
            Some(req.intake_id),
            Some(req.field),
            if existed { old_value } else { None },
            None,
            req.reason,
        );
        let audit_id = catalog.record_metadata_audit(&audit)?;

        Ok(write_outcome(ops, audit_id, existed))
    })
}

/// Suppress one field's extracted value without supplying a
/// replacement: writes a NULL override (a tombstone), so the field has
/// no effective value until a correct one is set. For the case where
/// the extracted value is known to be wrong and no right value is at
/// hand. The field must be one of [`EDITABLE_FIELDS`];
/// [`clear_metadata_field`] removes the tombstone and restores the
/// extracted value.
pub fn void_metadata_field<E: Embedder>(
    ops: &Ops<E>,
    req: VoidMetadataFieldRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "field": req.field,
        "reason": req.reason,
    });
    record_call_sync!(ops, "library.metadata.void", args, {
        require_editable(&req.field)?;
        let catalog = Catalog::open(ops.catalog_db())?;
        require_intake(&catalog, req.intake_id)?;

        let effective = catalog.effective_publication_attrs(req.intake_id, ItemKind::Book)?;
        let old_value = effective.get(&req.field).map(str::to_string);

        let caller = ops.effective_caller();
        catalog.set_override(&NewOverride::new(
            req.intake_id,
            ItemKind::Book,
            req.field.clone(),
            None,
            caller.actor_kind.as_str(),
        ))?;

        // `changed` reflects the effective view: voiding a field that
        // already had no effective value still records the tombstone
        // (it suppresses what a future rebuild would re-extract) but
        // changed nothing visible.
        let changed = old_value.is_some();
        let audit = build_audit(
            ops,
            "node_publication_attrs",
            "void",
            Some(req.intake_id),
            Some(req.field),
            old_value,
            None,
            req.reason,
        );
        let audit_id = catalog.record_metadata_audit(&audit)?;

        Ok(write_outcome(ops, audit_id, changed))
    })
}

/// Attribute a contributor to the book root with `origin = "user"`,
/// appended after the role's existing contributors. The role must be
/// one of [`CONTRIBUTOR_ROLES`]. User-origin rows survive a re-ingest:
/// the pipeline's contributor refresh replaces only extracted rows.
pub fn add_contributor<E: Embedder>(
    ops: &Ops<E>,
    req: AddContributorRequest,
) -> Result<AddContributorOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "role": req.role,
        "name": req.name,
        "nationality": req.nationality,
        "reason": req.reason,
    });
    record_call_sync!(ops, "library.metadata.contributor_add", args, {
        if !CONTRIBUTOR_ROLES.contains(&req.role.as_str()) {
            return Err(OpsError::UnknownContributorRole { role: req.role });
        }
        let name = req.name.trim();
        if name.is_empty() {
            return Err(OpsError::Other(eyre::eyre!(
                "contributor name must not be empty"
            )));
        }
        let catalog = Catalog::open(ops.catalog_db())?;
        require_intake(&catalog, req.intake_id)?;

        let existing = catalog.contributors_for_address(req.intake_id, ItemKind::Book)?;
        let ordinal = existing
            .iter()
            .filter(|c| c.role == req.role)
            .map(|c| c.ordinal)
            .max()
            .map_or(0, |m| m + 1);

        let mut new = NewContributor::new(
            req.intake_id,
            ItemKind::Book,
            &req.role,
            ordinal,
            "user",
            name,
        );
        if let Some(nationality) = req.nationality.as_deref() {
            new = new.nationality(nationality);
        }
        let contributor_id = catalog.add_contributor(&new)?;

        let audit = build_audit(
            ops,
            "node_contributors",
            "insert",
            Some(req.intake_id),
            Some(req.role.clone()),
            None,
            Some(name.to_string()),
            req.reason,
        );
        let audit_id = catalog.record_metadata_audit(&audit)?;

        Ok(AddContributorOutcome {
            contributor_id,
            write: write_outcome(ops, audit_id, true),
        })
    })
}

/// Remove one contributor row by its surrogate id, whatever its origin
/// — removing a wrong extracted attribution (e.g. an ebook packager
/// credited as the author) is the point. The row must belong to the
/// named book. A forced re-ingest re-seeds extracted rows, so a
/// removal of one may need repeating after it; user-origin rows are
/// unaffected.
pub fn remove_contributor<E: Embedder>(
    ops: &Ops<E>,
    req: RemoveContributorRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "contributor_id": req.contributor_id,
        "reason": req.reason,
    });
    record_call_sync!(ops, "library.metadata.contributor_remove", args, {
        let catalog = Catalog::open(ops.catalog_db())?;
        require_intake(&catalog, req.intake_id)?;

        let existing = catalog.contributors_for_address(req.intake_id, ItemKind::Book)?;
        let Some(row) = existing
            .into_iter()
            .find(|c| c.contributor_id == req.contributor_id)
        else {
            return Err(OpsError::ContributorNotFound {
                contributor_id: req.contributor_id,
                intake_id: req.intake_id,
            });
        };

        let removed = catalog.remove_contributor(req.contributor_id)?;

        let audit = build_audit(
            ops,
            "node_contributors",
            "delete",
            Some(req.intake_id),
            Some(row.role),
            Some(row.name),
            None,
            req.reason,
        );
        let audit_id = catalog.record_metadata_audit(&audit)?;

        Ok(write_outcome(ops, audit_id, removed))
    })
}

/// Re-run the metadata plausibility audit for one book from its cached
/// extraction envelope, refreshing the stored `confidence` /
/// `audit_verdict` rollup so it reflects the current effective
/// metadata (overrides included). The review status is untouched: the
/// audit is machine plausibility, review is human (or LLM)
/// confirmation.
pub fn reaudit_metadata<E: Embedder>(
    ops: &Ops<E>,
    req: ReauditMetadataRequest,
    audit_data: &bookrack_ingest::AuditData,
    audit_profile: &bookrack_ingest::AuditProfile,
) -> Result<ReauditOutcome> {
    let args = serde_json::json!({ "intake_id": req.intake_id });
    record_call_sync!(ops, "library.metadata.reaudit", args, {
        let catalog = Catalog::open(ops.catalog_db())?;
        let outcome = bookrack_ingest::reaudit::reaudit_book(
            &catalog,
            req.intake_id,
            audit_data,
            audit_profile,
        )
        .map_err(|e| match e {
            bookrack_ingest::IngestError::UnknownIntake(intake_id) => {
                OpsError::IntakeNotFound { intake_id }
            }
            other => OpsError::Other(eyre::Report::new(other)),
        })?;
        Ok(ReauditOutcome {
            intake_id: outcome.intake_id,
            previous_verdict: outcome.previous_verdict,
            previous_confidence: outcome.previous_confidence,
            verdict: outcome.verdict,
            confidence: outcome.confidence,
        })
    })
}

/// Acknowledge a metadata gap: leaves the audit verdict alone but flips
/// the review row to `acknowledged` with a recorded reason.
pub fn acknowledge_metadata_gap<E: Embedder>(
    ops: &Ops<E>,
    req: AcknowledgeMetadataGapRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "reason": req.reason,
    });
    record_call_sync!(ops, "library.metadata.ack", args, {
        let catalog = Catalog::open(ops.catalog_db())?;
        write_review_status_inner(
            ops,
            &catalog,
            ItemKind::Book,
            req.intake_id,
            STATUS_ACKNOWLEDGED,
            "acknowledge_gate",
            Some(req.reason.clone()),
        )
    })
}

/// Approve the record. The audit verdict is unchanged; the review row
/// is flipped to `approved`. The reason lands on the audit row only:
/// `node_reviews.notes` (the ingest audit's note) stays in place.
pub fn approve_metadata<E: Embedder>(
    ops: &Ops<E>,
    req: ApproveMetadataRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "reason": req.reason,
    });
    record_call_sync!(ops, "library.metadata.approve", args, {
        let catalog = Catalog::open(ops.catalog_db())?;
        write_review_status_inner(
            ops,
            &catalog,
            ItemKind::Book,
            req.intake_id,
            STATUS_APPROVED,
            "approve",
            req.reason.clone(),
        )
    })
}

/// Reject the book. Pipeline rows stay in place so downstream consumers
/// can filter on `rejected`. The reason lands on the audit row only:
/// `node_reviews.notes` (the ingest audit's note) stays in place.
pub fn reject_metadata<E: Embedder>(
    ops: &Ops<E>,
    req: RejectMetadataRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "reason": req.reason,
    });
    record_call_sync!(ops, "library.metadata.reject", args, {
        let catalog = Catalog::open(ops.catalog_db())?;
        write_review_status_inner(
            ops,
            &catalog,
            ItemKind::Book,
            req.intake_id,
            STATUS_REJECTED,
            "reject",
            Some(req.reason.clone()),
        )
    })
}

fn require_editable(field: &str) -> Result<()> {
    if !EDITABLE_FIELDS.contains(&field) {
        return Err(OpsError::UnknownMetadataField {
            field: field.to_string(),
        });
    }
    Ok(())
}
