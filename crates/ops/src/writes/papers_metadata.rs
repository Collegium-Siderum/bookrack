// SPDX-License-Identifier: Apache-2.0

//! Paper-side metadata write ops: override edits, contributor
//! attribution, and review-status transitions.
//!
//! One op per curation action, mirroring
//! [`crate::writes::metadata`] on the book side and sharing that
//! side's internals through [`crate::writes`]. Each op opens the paper
//! catalog read-write, applies its change, and appends one
//! [`bookrack_catalog::MetadataAudit`] row stamped with the effective
//! [`crate::Caller`], so a CLI edit and an MCP edit are
//! distinguishable by `actor_kind` / `actor_detail`.
//!
//! The catalog is opened through [`Catalog::open_with_backup`]: the
//! paper write path has no outer caller that migrates the schema
//! first, so this is the door that takes a backup before a migration
//! runs.

use bookrack_catalog::{
    CONTRIBUTOR_ROLES, Catalog, NewContributor, NewOverride, STATUS_ACKNOWLEDGED, STATUS_APPROVED,
    STATUS_PENDING, STATUS_REJECTED,
};
use bookrack_core::ItemKind;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::writes::{
    AddContributorOutcome, PaperClearMetadataFieldRequest, PaperContributorAddRequest,
    PaperContributorRemoveRequest, PaperReviewRequest, PaperSetMetadataFieldRequest,
    PaperVoidMetadataFieldRequest, WriteOutcome,
};
use crate::recorder::record_call_sync;
use crate::writes::{build_audit, require_intake, write_outcome, write_review_status_inner};

/// Fields the paper-side curation surface accepts under
/// [`set_paper_metadata_field`] and [`void_paper_metadata_field`].
///
/// A strict subset of [`bookrack_catalog::EDITABLE_FIELDS`]: the paper
/// pipeline stores no `isbn`, `edition`, or `pub_place`, and an
/// override on a column the paper effective view never reads would be
/// invisible to every consumer, the audit included.
pub const PAPER_EDITABLE_FIELDS: &[&str] = &[
    "title",
    "subtitle",
    "publisher",
    "year",
    "language",
    "series",
    "doi",
    "arxiv_id",
    "issn",
    "container_title",
    "abstract_text",
    "csl_type",
];

/// Open the paper catalog this `Ops` is configured against.
fn open_paper_catalog<E: Embedder>(ops: &Ops<E>) -> Result<Catalog> {
    let catalog_db = ops
        .papers_catalog_db()
        .ok_or(OpsError::PapersBackendNotConfigured)?;
    Ok(Catalog::open_with_backup(catalog_db, ops.backup_dir())?)
}

fn require_paper_editable(field: &str) -> Result<()> {
    if !PAPER_EDITABLE_FIELDS.contains(&field) {
        return Err(OpsError::UnknownMetadataField {
            field: field.to_string(),
            editable: PAPER_EDITABLE_FIELDS
                .iter()
                .map(|f| f.to_string())
                .collect(),
        });
    }
    Ok(())
}

/// Set an override on one bibliographic field of a paper, writing the
/// audit row that records the change. The field must be one of
/// [`PAPER_EDITABLE_FIELDS`]; an unknown name is rejected before
/// anything is written.
pub fn set_paper_metadata_field<E: Embedder>(
    ops: &Ops<E>,
    req: PaperSetMetadataFieldRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "field": req.field,
        "value": req.value,
        "reason": req.reason,
        "confirmed": req.confirmed,
    });
    record_call_sync!(ops, "papers.metadata.set", args, {
        require_paper_editable(&req.field)?;
        let catalog = open_paper_catalog(ops)?;
        require_intake(&catalog, req.intake_id)?;

        let effective = catalog.effective_publication_attrs(req.intake_id, ItemKind::Paper)?;
        let old_value = effective.get(&req.field).map(str::to_string);

        let caller = ops.effective_caller();
        catalog.set_override(
            &NewOverride::new(
                req.intake_id,
                ItemKind::Paper,
                req.field.clone(),
                Some(req.value.clone()),
                caller.actor_kind.as_str(),
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

/// Remove an override on one paper field, reverting to the extracted
/// value.
///
/// The field name is looser here than on [`set_paper_metadata_field`]:
/// a name outside [`PAPER_EDITABLE_FIELDS`] is accepted when an
/// override row with that key exists — rows that predate validation
/// must stay removable — and rejected when there is nothing to remove.
pub fn clear_paper_metadata_field<E: Embedder>(
    ops: &Ops<E>,
    req: PaperClearMetadataFieldRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "field": req.field,
        "reason": req.reason,
    });
    record_call_sync!(ops, "papers.metadata.clear", args, {
        let catalog = open_paper_catalog(ops)?;
        require_intake(&catalog, req.intake_id)?;

        let effective = catalog.effective_publication_attrs(req.intake_id, ItemKind::Paper)?;
        let old_value = effective.get(&req.field).map(str::to_string);

        let existed = catalog.clear_override(req.intake_id, ItemKind::Paper, &req.field)?;
        if !existed {
            require_paper_editable(&req.field)?;
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

/// Suppress one paper field's extracted value without supplying a
/// replacement: writes a NULL override (a tombstone), so the field has
/// no effective value until a correct one is set.
/// [`clear_paper_metadata_field`] removes the tombstone and restores
/// the extracted value.
pub fn void_paper_metadata_field<E: Embedder>(
    ops: &Ops<E>,
    req: PaperVoidMetadataFieldRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "field": req.field,
        "reason": req.reason,
    });
    record_call_sync!(ops, "papers.metadata.void", args, {
        require_paper_editable(&req.field)?;
        let catalog = open_paper_catalog(ops)?;
        require_intake(&catalog, req.intake_id)?;

        let effective = catalog.effective_publication_attrs(req.intake_id, ItemKind::Paper)?;
        let old_value = effective.get(&req.field).map(str::to_string);

        let caller = ops.effective_caller();
        catalog.set_override(&NewOverride::new(
            req.intake_id,
            ItemKind::Paper,
            req.field.clone(),
            None,
            caller.actor_kind.as_str(),
        ))?;

        // `changed` reflects the effective view: voiding a field that
        // already had no effective value still records the tombstone
        // but changed nothing visible.
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

/// Next free ordinal for a curator-added contributor in `role`.
///
/// The `(intake_id, scope, role, ordinal, origin)` UNIQUE key on
/// `node_contributors` makes any `existing.len()`-based formula unsafe
/// once a row in `role` has been removed: with `[0, 1, 2]` and `0`
/// gone, the length is still `2`, so the next insert would collide on
/// `ordinal = 2`.
fn next_contributor_ordinal(existing: &[bookrack_catalog::NodeContributor], role: &str) -> i64 {
    existing
        .iter()
        .filter(|c| c.role == role)
        .map(|c| c.ordinal)
        .max()
        .map_or(0, |m| m + 1)
}

/// Attribute a contributor to a paper with `origin = "user"`, appended
/// after the role's existing contributors. The role must be one of
/// [`bookrack_catalog::CONTRIBUTOR_ROLES`].
///
/// The paper shape carries the structured name parts and the ORCID the
/// citation formats need, and no nationality — that one is a book-side
/// enrichment field.
pub fn add_paper_contributor<E: Embedder>(
    ops: &Ops<E>,
    req: PaperContributorAddRequest,
) -> Result<AddContributorOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "role": req.role,
        "name": req.name,
        "family": req.family,
        "given": req.given,
        "orcid": req.orcid,
        "reason": req.reason,
    });
    record_call_sync!(ops, "papers.metadata.contributor_add", args, {
        if !CONTRIBUTOR_ROLES.contains(&req.role.as_str()) {
            return Err(OpsError::UnknownContributorRole { role: req.role });
        }
        let name = req.name.trim();
        if name.is_empty() {
            return Err(OpsError::Other(eyre::eyre!(
                "contributor name must not be empty"
            )));
        }
        let catalog = open_paper_catalog(ops)?;
        require_intake(&catalog, req.intake_id)?;

        let existing = catalog.contributors_for_address(req.intake_id, ItemKind::Paper)?;
        let ordinal = next_contributor_ordinal(&existing, &req.role);

        let mut new = NewContributor::new(
            req.intake_id,
            ItemKind::Paper,
            &req.role,
            ordinal,
            "user",
            name,
        );
        if let Some(family) = req.family.as_deref() {
            new = new.family(family);
        }
        if let Some(given) = req.given.as_deref() {
            new = new.given(given);
        }
        if let Some(orcid) = req.orcid.as_deref() {
            new = new.orcid(orcid);
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

/// Remove one contributor row from a paper by its surrogate id,
/// whatever its origin. The row must belong to the named paper: the id
/// alone addresses a row anywhere in the catalog, so the paper is what
/// makes the request checkable.
pub fn remove_paper_contributor<E: Embedder>(
    ops: &Ops<E>,
    req: PaperContributorRemoveRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({
        "intake_id": req.intake_id,
        "contributor_id": req.contributor_id,
        "reason": req.reason,
    });
    record_call_sync!(ops, "papers.metadata.contributor_remove", args, {
        let catalog = open_paper_catalog(ops)?;
        require_intake(&catalog, req.intake_id)?;

        let existing = catalog.contributors_for_address(req.intake_id, ItemKind::Paper)?;
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

/// Acknowledge a metadata gap on a paper: leaves the audit verdict
/// alone but flips the review row to `acknowledged`.
pub fn acknowledge_paper_metadata_gap<E: Embedder>(
    ops: &Ops<E>,
    req: PaperReviewRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({ "intake_id": req.intake_id });
    record_call_sync!(ops, "papers.metadata.ack", args, {
        let catalog = open_paper_catalog(ops)?;
        write_review_status_inner(
            ops,
            &catalog,
            ItemKind::Paper,
            req.intake_id,
            STATUS_ACKNOWLEDGED,
            "acknowledge_gate",
            None,
        )
    })
}

/// Approve a paper's record. The audit verdict is unchanged; the
/// review row is flipped to `approved`.
pub fn approve_paper_metadata<E: Embedder>(
    ops: &Ops<E>,
    req: PaperReviewRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({ "intake_id": req.intake_id });
    record_call_sync!(ops, "papers.metadata.approve", args, {
        let catalog = open_paper_catalog(ops)?;
        write_review_status_inner(
            ops,
            &catalog,
            ItemKind::Paper,
            req.intake_id,
            STATUS_APPROVED,
            "approve",
            None,
        )
    })
}

/// Reject a paper. Pipeline rows stay in place so downstream consumers
/// can filter on `rejected`.
pub fn reject_paper_metadata<E: Embedder>(
    ops: &Ops<E>,
    req: PaperReviewRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({ "intake_id": req.intake_id });
    record_call_sync!(ops, "papers.metadata.reject", args, {
        let catalog = open_paper_catalog(ops)?;
        write_review_status_inner(
            ops,
            &catalog,
            ItemKind::Paper,
            req.intake_id,
            STATUS_REJECTED,
            "reject",
            None,
        )
    })
}

/// Demote a paper's review row back to `pending`, so it surfaces in
/// the review queue again. The book side has no matching verb.
pub fn reopen_paper_review<E: Embedder>(
    ops: &Ops<E>,
    req: PaperReviewRequest,
) -> Result<WriteOutcome> {
    let args = serde_json::json!({ "intake_id": req.intake_id });
    record_call_sync!(ops, "papers.metadata.reopen", args, {
        let catalog = open_paper_catalog(ops)?;
        write_review_status_inner(
            ops,
            &catalog,
            ItemKind::Paper,
            req.intake_id,
            STATUS_PENDING,
            "reopen",
            None,
        )
    })
}

#[cfg(test)]
mod tests {
    use bookrack_catalog::{Catalog, NewContributor};
    use bookrack_core::ItemKind;

    use super::{PAPER_EDITABLE_FIELDS, next_contributor_ordinal};

    /// Every field this surface accepts must name a real
    /// effective-attrs field, or the stored override is invisible to
    /// every effective-view consumer (including the audit).
    #[test]
    fn paper_editable_fields_are_a_subset_of_the_catalog_editable_set() {
        for field in PAPER_EDITABLE_FIELDS {
            assert!(
                bookrack_catalog::EDITABLE_FIELDS.contains(field),
                "{field} is not a catalog editable field",
            );
        }
    }

    /// End-to-end against a paper-side catalog: insert three author
    /// rows in (0, 1, 2), remove the one at `ordinal = 0`, then add a
    /// fourth. An `existing.len()` formula picks `ordinal = 2` (the
    /// new length) and the insert fails on the UNIQUE key; the
    /// max-plus-one formula picks `ordinal = 3` and the insert
    /// succeeds.
    #[test]
    fn add_after_remove_does_not_collide_with_surviving_ordinal() {
        let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
        let intake = 1_i64;
        let kind = ItemKind::Paper;
        let role = "author";
        let origin = "user";

        let mut ids = Vec::with_capacity(3);
        for (ord, name) in [(0, "a"), (1, "b"), (2, "c")] {
            let id = catalog
                .add_contributor(&NewContributor::new(intake, kind, role, ord, origin, name))
                .expect("seed contributor");
            ids.push(id);
        }

        assert!(
            catalog.remove_contributor(ids[0]).expect("remove ord=0"),
            "remove must report a deleted row"
        );

        let existing = catalog
            .contributors_for_address(intake, kind)
            .expect("read existing");
        let ordinal = next_contributor_ordinal(&existing, role);
        assert_eq!(ordinal, 3, "next ordinal must be max(1, 2) + 1");

        catalog
            .add_contributor(&NewContributor::new(
                intake, kind, role, ordinal, origin, "d",
            ))
            .expect("add after remove must not collide on the UNIQUE key");
    }

    #[test]
    fn next_ordinal_is_scoped_to_the_role_and_starts_at_zero() {
        let rows = |specs: &[(&str, i64)]| -> Vec<bookrack_catalog::NodeContributor> {
            specs
                .iter()
                .map(|(role, ordinal)| bookrack_catalog::NodeContributor {
                    contributor_id: 0,
                    intake_id: 0,
                    scope: ItemKind::Paper.as_scope_str().to_string(),
                    role: (*role).to_string(),
                    ordinal: *ordinal,
                    origin: "user".to_string(),
                    name: "x".to_string(),
                    nationality: None,
                    inheritable: true,
                    family: None,
                    given: None,
                    orcid: None,
                })
                .collect()
        };
        let existing = rows(&[("author", 0), ("author", 1), ("editor", 5), ("editor", 7)]);
        assert_eq!(next_contributor_ordinal(&existing, "author"), 2);
        assert_eq!(next_contributor_ordinal(&existing, "editor"), 8);
        assert_eq!(next_contributor_ordinal(&existing, "translator"), 0);
    }
}
