// SPDX-License-Identifier: Apache-2.0

//! Paper-side metadata curation methods on the control plane.
//!
//! Exposes the same nine actions the books pipeline does — `reaudit`,
//! `set`, `clear`, `void`, `ack`, `approve`, `reject`,
//! `contributor_add`, `contributor_remove` — but with paper-shape
//! semantics and paper-only stores. Each method runs its body through
//! [`super::run_write`], which holds the daemon's write mutex, raises
//! the write source, pauses MCP for the duration, drives the body on a
//! blocking executor, and announces the library it changed. Inside
//! that, the method opens the paper catalog via the library handle and
//! dispatches to a thin `bookrack-catalog` write. An audit trail row
//! will land in a follow-up; the writes themselves are durable.

use std::collections::HashSet;
use std::sync::LazyLock;

use bookrack_catalog::{
    Catalog, NewContributor, NewOverride, NewReview, NodeContributor, STATUS_ACKNOWLEDGED,
    STATUS_APPROVED, STATUS_PENDING, STATUS_REJECTED,
};
use bookrack_core::ItemKind;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MethodContext, input_err, run_write};
use crate::audit_helpers::{
    load_paper_audit_data, load_paper_audit_profile, require_known_profile,
};
use crate::cmd::input_error::CmdInputError;
use crate::control::error_map::{registry_err, write_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

const PAPER_SCOPE: &str = "paper";

/// Fields the paper-side metadata write surface accepts under
/// `papers.metadata.set` / `void`. Mirrors the columns paper writes
/// land on in `node_publication_attrs`.
static EDITABLE_FIELDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
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
    ]
    .into_iter()
    .collect()
});

fn parse<T: for<'de> Deserialize<'de>>(
    params: &Option<Value>,
    method: &str,
) -> Result<T, RpcError> {
    match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid {method} params: {e}"))),
        _ => Err(RpcError::new(
            INVALID_PARAMS,
            format!("missing {method} params"),
        )),
    }
}

/// Run one paper-metadata curation write through the daemon's write
/// path, with the target library's paper catalog already open.
///
/// Everything a write needs beyond the SQL itself comes from
/// [`run_write`]: the mutex that serializes this against an ingest or
/// glean writing the same catalog, the MCP pause, the blocking
/// executor the synchronous sqlite work belongs on, and the
/// `LibraryChanged` broadcast that tells a subscriber to refresh the
/// library this call named.
///
/// The catalog is opened inside the write body rather than handed in,
/// so the handle is not held across the mutex acquisition.
async fn run_paper_metadata_write<F>(
    ctx: &MethodContext,
    library: Option<&str>,
    op: F,
) -> Result<Value, RpcError>
where
    F: FnOnce(Catalog) -> Result<Value, RpcError> + Send + 'static,
{
    let handle = ctx.registry.get(library).map_err(registry_err)?;
    let library_name = handle.name().to_string();
    run_write(ctx, &library_name, move || async move {
        let catalog = handle
            .open_paper_catalog()
            .map_err(|e| write_err("papers.metadata", e))?;
        op(catalog)
    })
    .await
}

/// Refuse a write addressed at an intake the paper catalog does not
/// hold.
///
/// The paper side has no ops write layer, so the guard the book side
/// gets from `ops::writes::metadata::require_intake` has to sit beside
/// the handlers instead. Without it the write lands: the override,
/// review, and contributor tables carry no foreign key onto
/// `intakes`, so a phantom id becomes a row nothing ever reads and
/// `remove` never cascades away.
///
/// The lookup's own failure is not folded into `UnknownIntake`.
/// "The catalog says no" and "the catalog could not be asked" call for
/// different next steps, and reporting a disk fault as operator error
/// is the same mistake this guard exists to fix.
fn require_paper_intake(catalog: &Catalog, intake_id: i64) -> Result<(), RpcError> {
    match catalog.intake_by_id(intake_id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(input_err(CmdInputError::UnknownIntake { intake_id })),
        Err(e) => Err(RpcError::new(
            INTERNAL_ERROR,
            format!("intake lookup: {}", bookrack_core::error_chain(&e)),
        )),
    }
}

fn require_editable(field: &str) -> Result<(), RpcError> {
    if EDITABLE_FIELDS.contains(field) {
        Ok(())
    } else {
        Err(RpcError::new(
            INVALID_PARAMS,
            format!("field {field:?} is not a paper editable field"),
        ))
    }
}

// ─── reaudit ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PapersMetadataReauditParams {
    intake_id: i64,
    /// Optional paper-side audit profile name. Absent means the
    /// overlay-resolved default; a name in the paper-side built-in set
    /// (`default` / `trust-source` / `strict`) selects that built-in;
    /// any other name is refused as invalid params. The paper set is
    /// checked separately from the book one.
    #[serde(default)]
    audit_profile: Option<String>,
    #[serde(default)]
    library: Option<String>,
}

pub async fn reaudit(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersMetadataReauditParams = parse(params, "papers.metadata.reaudit")?;
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    require_known_profile(
        parsed.audit_profile.as_deref(),
        bookrack_glean::audit::profile::ALL_BUILT_IN_NAMES,
    )
    .map_err(input_err)?;
    let library_name = handle.name().to_string();
    let PapersMetadataReauditParams {
        intake_id,
        audit_profile,
        ..
    } = parsed;
    run_write(ctx, &library_name, move || async move {
        // Both overlays live under the target library's data root, so
        // they are read from the handle this call resolved — the
        // catalog below is that library's, and auditing it under
        // another library's rules is the asymmetry this pairing
        // removes.
        let profile = load_paper_audit_profile(handle.cfg(), audit_profile.as_deref());
        let data = load_paper_audit_data(handle.cfg());
        let outcome = handle
            .reaudit_paper(intake_id, &profile, &data)
            .await
            .map_err(|e| write_err("papers.metadata.reaudit", e))?;
        Ok(json!({
            "intake_id": outcome.intake_id,
            "verdict": outcome.verdict,
            "previous_verdict": outcome.previous_verdict,
            "confidence": outcome.confidence,
            "previous_confidence": outcome.previous_confidence,
        }))
    })
    .await
}

// ─── set / clear / void ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PapersMetadataSetParams {
    intake_id: i64,
    field: String,
    value: String,
    #[serde(default)]
    confirmed: bool,
    #[serde(default)]
    library: Option<String>,
}

pub async fn set(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersMetadataSetParams = parse(params, "papers.metadata.set")?;
    require_editable(&parsed.field)?;
    let library = parsed.library.clone();
    run_paper_metadata_write(ctx, library.as_deref(), move |catalog| {
        require_paper_intake(&catalog, parsed.intake_id)?;
        catalog
            .set_override(
                &NewOverride::new(
                    parsed.intake_id,
                    ItemKind::Paper,
                    &parsed.field,
                    Some(parsed.value.clone()),
                    "human",
                )
                .confirmed(parsed.confirmed),
            )
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("set_override: {e}")))?;
        Ok(json!({
            "intake_id": parsed.intake_id,
            "field": parsed.field,
            "value": parsed.value,
            "confirmed": parsed.confirmed,
        }))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct PapersMetadataClearParams {
    intake_id: i64,
    field: String,
    #[serde(default)]
    library: Option<String>,
}

pub async fn clear(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersMetadataClearParams = parse(params, "papers.metadata.clear")?;
    let library = parsed.library.clone();
    run_paper_metadata_write(ctx, library.as_deref(), move |catalog| {
        require_paper_intake(&catalog, parsed.intake_id)?;
        let removed = catalog
            .clear_override(parsed.intake_id, ItemKind::Paper, &parsed.field)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("clear_override: {e}")))?;
        Ok(json!({
            "intake_id": parsed.intake_id,
            "field": parsed.field,
            "removed": removed,
        }))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct PapersMetadataVoidParams {
    intake_id: i64,
    field: String,
    #[serde(default)]
    library: Option<String>,
}

pub async fn void(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersMetadataVoidParams = parse(params, "papers.metadata.void")?;
    require_editable(&parsed.field)?;
    let library = parsed.library.clone();
    run_paper_metadata_write(ctx, library.as_deref(), move |catalog| {
        require_paper_intake(&catalog, parsed.intake_id)?;
        catalog
            .set_override(&NewOverride::new(
                parsed.intake_id,
                ItemKind::Paper,
                &parsed.field,
                None,
                "human",
            ))
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("set_override: {e}")))?;
        Ok(json!({
            "intake_id": parsed.intake_id,
            "field": parsed.field,
            "voided": true,
        }))
    })
    .await
}

// ─── ack / approve / reject ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PapersReviewParams {
    intake_id: i64,
    #[serde(default)]
    reviewer: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    library: Option<String>,
}

async fn write_review_status(
    ctx: &MethodContext,
    parsed: PapersReviewParams,
    status: &'static str,
) -> Result<Value, RpcError> {
    let library = parsed.library.clone();
    run_paper_metadata_write(ctx, library.as_deref(), move |catalog| {
        require_paper_intake(&catalog, parsed.intake_id)?;
        let reviewer = parsed.reviewer.as_deref().unwrap_or("human");
        let mut review = NewReview::new(parsed.intake_id, ItemKind::Paper, reviewer, status);
        if let Some(notes) = parsed.notes.as_deref() {
            review = review.notes(notes);
        }
        catalog
            .upsert_review(&review)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("upsert_review: {e}")))?;
        Ok(json!({
            "intake_id": parsed.intake_id,
            "status": status,
            "reviewer": reviewer,
        }))
    })
    .await
}

pub async fn ack(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    write_review_status(
        ctx,
        parse(params, "papers.metadata.ack")?,
        STATUS_ACKNOWLEDGED,
    )
    .await
}

pub async fn approve(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    write_review_status(
        ctx,
        parse(params, "papers.metadata.approve")?,
        STATUS_APPROVED,
    )
    .await
}

pub async fn reject(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    write_review_status(
        ctx,
        parse(params, "papers.metadata.reject")?,
        STATUS_REJECTED,
    )
    .await
}

/// Demote the review row back to `pending`. Useful when an
/// `approve` / `reject` was wrong and the operator wants the row to
/// surface in the queue again.
pub async fn reopen(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    write_review_status(
        ctx,
        parse(params, "papers.metadata.reopen")?,
        STATUS_PENDING,
    )
    .await
}

// ─── contributor_add / contributor_remove ───────────────────────────

#[derive(Debug, Deserialize)]
pub struct PapersContributorAddParams {
    intake_id: i64,
    role: String,
    name: String,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    given: Option<String>,
    #[serde(default)]
    orcid: Option<String>,
    #[serde(default)]
    library: Option<String>,
}

/// Next free ordinal for a curator-added contributor in `role`.
///
/// The `(intake_id, scope, role, ordinal, origin)` UNIQUE key on
/// `node_contributors` makes any `existing.len()`-based formula unsafe
/// once a row in `role` has been removed: with `[0, 1, 2]` and `0`
/// gone, the length is still `2`, so the next insert would collide on
/// `ordinal = 2`. Scoping the `max` to the same role mirrors the book
/// side of the writer surface.
fn next_contributor_ordinal(existing: &[NodeContributor], role: &str) -> i64 {
    existing
        .iter()
        .filter(|c| c.role == role)
        .map(|c| c.ordinal)
        .max()
        .map_or(0, |m| m + 1)
}

pub async fn contributor_add(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let parsed: PapersContributorAddParams = parse(params, "papers.metadata.contributor_add")?;
    let library = parsed.library.clone();
    run_paper_metadata_write(ctx, library.as_deref(), move |catalog| {
        require_paper_intake(&catalog, parsed.intake_id)?;
        // Place curator-added contributors after every other row in the
        // same role. Computing `existing.len()` instead would collide on
        // the `(intake_id, scope, role, ordinal, origin)` UNIQUE key as
        // soon as any prior row had been removed: the length no longer
        // matches the next free position.
        let existing = catalog
            .contributors_for_address(parsed.intake_id, ItemKind::Paper)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("contributors_for_address: {e}")))?;
        let ordinal = next_contributor_ordinal(&existing, &parsed.role);
        let mut new = NewContributor::new(
            parsed.intake_id,
            ItemKind::Paper,
            &parsed.role,
            ordinal,
            "human",
            &parsed.name,
        );
        if let Some(family) = parsed.family.as_deref() {
            new = new.family(family);
        }
        if let Some(given) = parsed.given.as_deref() {
            new = new.given(given);
        }
        if let Some(orcid) = parsed.orcid.as_deref() {
            new = new.orcid(orcid);
        }
        let contributor_id = catalog
            .add_contributor(&new)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("add_contributor: {e}")))?;
        Ok(json!({
            "intake_id": parsed.intake_id,
            "contributor_id": contributor_id,
            "role": parsed.role,
            "name": parsed.name,
        }))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct PapersContributorRemoveParams {
    contributor_id: i64,
    #[serde(default)]
    library: Option<String>,
}

pub async fn contributor_remove(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let parsed: PapersContributorRemoveParams =
        parse(params, "papers.metadata.contributor_remove")?;
    let library = parsed.library.clone();
    run_paper_metadata_write(ctx, library.as_deref(), move |catalog| {
        let removed = catalog
            .remove_contributor(parsed.contributor_id)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("remove_contributor: {e}")))?;
        Ok(json!({
            "contributor_id": parsed.contributor_id,
            "removed": removed,
        }))
    })
    .await
}

/// Marker so the unused-`PAPER_SCOPE` constant can be removed in a
/// follow-up that introduces a scope-aware metadata listing. Kept as
/// a public-facing label that downstream renderers can call out.
pub const SCOPE: &str = PAPER_SCOPE;

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize a single existing-row record carrying only the
    /// fields `next_contributor_ordinal` consults.
    fn row(role: &str, ordinal: i64) -> NodeContributor {
        NodeContributor {
            contributor_id: 0,
            intake_id: 0,
            scope: ItemKind::Paper.as_scope_str().to_string(),
            role: role.to_string(),
            ordinal,
            origin: "human".to_string(),
            name: "x".to_string(),
            nationality: None,
            inheritable: true,
            family: None,
            given: None,
            orcid: None,
        }
    }

    #[test]
    fn paper_editable_fields_are_a_subset_of_the_catalog_editable_set() {
        // Every field this surface accepts must name a real
        // effective-attrs field, or the stored override is invisible
        // to every effective-view consumer (including the audit).
        for field in EDITABLE_FIELDS.iter() {
            assert!(
                bookrack_catalog::EDITABLE_FIELDS.contains(field),
                "{field} is not a catalog editable field",
            );
        }
    }

    #[test]
    fn next_ordinal_is_zero_when_role_is_empty() {
        let existing: Vec<NodeContributor> = vec![row("editor", 0), row("editor", 1)];
        assert_eq!(next_contributor_ordinal(&existing, "author"), 0);
    }

    /// The previous `existing.len()` formula returned `2` after
    /// `ordinal=0` was removed, which then collided with the still-
    /// present `ordinal=2` row on the UNIQUE key.
    #[test]
    fn next_ordinal_uses_max_plus_one_not_length() {
        let existing = vec![row("author", 1), row("author", 2)];
        assert_eq!(next_contributor_ordinal(&existing, "author"), 3);
    }

    #[test]
    fn next_ordinal_ignores_other_roles() {
        let existing = vec![
            row("author", 0),
            row("author", 1),
            row("editor", 5),
            row("editor", 7),
        ];
        assert_eq!(next_contributor_ordinal(&existing, "author"), 2);
        assert_eq!(next_contributor_ordinal(&existing, "editor"), 8);
        assert_eq!(next_contributor_ordinal(&existing, "translator"), 0);
    }

    /// End-to-end against a paper-side catalog: insert three author
    /// rows in (0, 1, 2), remove the one at `ordinal = 0`, then add a
    /// fourth. The previous formula picked `ordinal = 2` (the new
    /// `existing.len()`) and the insert failed on the UNIQUE key; the
    /// new formula picks `ordinal = 3` and the insert succeeds.
    #[test]
    fn add_after_remove_does_not_collide_with_surviving_ordinal() {
        let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
        let intake = 1_i64;
        let kind = ItemKind::Paper;
        let role = "author";
        let origin = "human";

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
}
