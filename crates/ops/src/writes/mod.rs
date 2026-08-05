// SPDX-License-Identifier: Apache-2.0

//! Write ops over the bookrack library.
//!
//! Each write op opens the catalog read-write, applies the change, and
//! records a [`bookrack_catalog::MetadataAudit`] row tagged with the
//! [`crate::Caller`] this [`crate::Ops`] was built with.
//!
//! The pieces every curation write shares — the intake guard, the audit
//! row builder, the outcome builder, and the review-status body — live
//! here at crate scope so the per-pipeline modules hold only what
//! differs between them.

pub mod metadata;

use bookrack_catalog::{Catalog, NewMetadataAudit, NewReview};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::writes::WriteOutcome;

/// Refuse a write addressed at an intake the catalog does not hold.
///
/// The override, review, and contributor tables carry no foreign key
/// onto `intakes`, so a phantom id becomes a row nothing ever reads and
/// `remove` never cascades away.
///
/// The lookup's own failure is not folded into
/// [`OpsError::IntakeNotFound`]: "the catalog says no" and "the catalog
/// could not be asked" call for different next steps.
pub(crate) fn require_intake(catalog: &Catalog, intake_id: i64) -> Result<()> {
    if catalog.intake_by_id(intake_id)?.is_none() {
        return Err(OpsError::IntakeNotFound { intake_id });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // Mirrors the columns of NewMetadataAudit; collapsing into a builder would just hide the same field list.
pub(crate) fn build_audit<E: Embedder>(
    ops: &Ops<E>,
    table_name: &str,
    action: &str,
    intake_id: Option<i64>,
    field: Option<String>,
    old_value: Option<String>,
    new_value: Option<String>,
    reason: Option<String>,
) -> NewMetadataAudit {
    let caller = ops.effective_caller();
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

pub(crate) fn write_outcome<E: Embedder>(
    ops: &Ops<E>,
    audit_id: i64,
    changed: bool,
) -> WriteOutcome {
    let caller = ops.effective_caller();
    WriteOutcome {
        audit_id,
        actor_kind: caller.actor_kind.as_str().to_string(),
        actor_detail: caller.actor_detail.clone(),
        changed,
    }
}

/// Move one item's review row to `status`, appending the audit row that
/// records the transition.
///
/// The audit row lands before the review row so a failure between the
/// two leaves the trail saying an attempt was made rather than a status
/// with no record of who set it. `reason` is optional here; which verbs
/// require one is decided by the request DTO of each op.
///
/// `node_reviews.notes` is not touched: it carries the ingest audit's
/// report, and the curator's words belong on the audit row.
pub(crate) fn write_review_status_inner<E: Embedder>(
    ops: &Ops<E>,
    catalog: &Catalog,
    kind: ItemKind,
    intake_id: i64,
    status: &'static str,
    action: &'static str,
    reason: Option<String>,
) -> Result<WriteOutcome> {
    require_intake(catalog, intake_id)?;

    let audit = build_audit(
        ops,
        "node_reviews",
        action,
        Some(intake_id),
        None,
        None,
        None,
        reason,
    );
    let audit_id = catalog.record_metadata_audit(&audit)?;

    let caller = ops.effective_caller();
    catalog.upsert_review(&NewReview::new(
        intake_id,
        kind,
        caller.actor_kind.as_str(),
        status,
    ))?;

    Ok(write_outcome(ops, audit_id, true))
}
