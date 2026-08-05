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
//! that, the method calls the matching op in
//! [`bookrack_ops::writes::papers_metadata`], which opens the paper
//! catalog, applies the change, and appends the `metadata_audit` row
//! that records who made it.

use bookrack_ops::dto::writes::{
    PaperClearMetadataFieldRequest, PaperContributorAddRequest, PaperContributorRemoveRequest,
    PaperReviewRequest, PaperSetMetadataFieldRequest, PaperVoidMetadataFieldRequest,
};
use bookrack_ops::writes::papers_metadata as ops_papers;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MethodContext, input_err, run_write};
use crate::audit_helpers::{
    load_paper_audit_data, load_paper_audit_profile, require_known_profile,
};
use crate::control::error_map::{registry_err, write_err};
use crate::control::jsonrpc::{INVALID_PARAMS, RpcError};

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
/// path, with the target library's ops handle resolved.
///
/// Everything a write needs beyond the change itself comes from
/// [`run_write`]: the mutex that serializes this against an ingest or
/// glean writing the same catalog, the MCP pause, the blocking
/// executor the synchronous sqlite work belongs on, and the
/// `LibraryChanged` broadcast that tells a subscriber to refresh the
/// library this call named.
///
/// The op opens the catalog itself rather than being handed one, so no
/// database handle is held across the mutex acquisition.
async fn run_paper_metadata_write<F>(
    ctx: &MethodContext,
    library: Option<&str>,
    method: &'static str,
    op: F,
) -> Result<Value, RpcError>
where
    F: FnOnce(&bookrack_ops::Ops<bookrack_embed::OllamaEmbedClient>) -> bookrack_ops::Result<Value>
        + Send
        + 'static,
{
    let handle = ctx.registry.get(library).map_err(registry_err)?;
    let library_name = handle.name().to_string();
    run_write(ctx, &library_name, move || async move {
        op(handle.ops()).map_err(|e| write_err(method, e.into()))
    })
    .await
}

/// Render a write outcome the way the book side does: the audit
/// identity of the edit, so a caller can cite the row without a second
/// round-trip.
fn outcome_json(outcome: &bookrack_ops::dto::writes::WriteOutcome) -> Value {
    json!({
        "audit_id": outcome.audit_id,
        "actor_kind": outcome.actor_kind,
        "actor_detail": outcome.actor_detail,
        "changed": outcome.changed,
    })
}

// ─── reaudit ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
            .map_err(|e| write_err("papers.metadata.reaudit", e.into()))?;
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
#[serde(deny_unknown_fields)]
pub struct PapersMetadataSetParams {
    intake_id: i64,
    field: String,
    value: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    confirmed: bool,
    #[serde(default)]
    library: Option<String>,
}

pub async fn set(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersMetadataSetParams = parse(params, "papers.metadata.set")?;
    let library = parsed.library.clone();
    run_paper_metadata_write(ctx, library.as_deref(), "papers.metadata.set", move |ops| {
        let field = parsed.field.clone();
        let value = parsed.value.clone();
        let outcome = ops_papers::set_paper_metadata_field(
            ops,
            PaperSetMetadataFieldRequest {
                intake_id: parsed.intake_id,
                field: parsed.field,
                value: parsed.value,
                reason: parsed.reason,
                confirmed: parsed.confirmed,
            },
        )?;
        let mut body = outcome_json(&outcome);
        body["intake_id"] = json!(parsed.intake_id);
        body["field"] = json!(field);
        body["value"] = json!(value);
        body["confirmed"] = json!(parsed.confirmed);
        Ok(body)
    })
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PapersMetadataClearParams {
    intake_id: i64,
    field: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    library: Option<String>,
}

pub async fn clear(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersMetadataClearParams = parse(params, "papers.metadata.clear")?;
    let library = parsed.library.clone();
    run_paper_metadata_write(
        ctx,
        library.as_deref(),
        "papers.metadata.clear",
        move |ops| {
            let field = parsed.field.clone();
            let outcome = ops_papers::clear_paper_metadata_field(
                ops,
                PaperClearMetadataFieldRequest {
                    intake_id: parsed.intake_id,
                    field: parsed.field,
                    reason: parsed.reason,
                },
            )?;
            let mut body = outcome_json(&outcome);
            body["intake_id"] = json!(parsed.intake_id);
            body["field"] = json!(field);
            body["removed"] = json!(outcome.changed);
            Ok(body)
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PapersMetadataVoidParams {
    intake_id: i64,
    field: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    library: Option<String>,
}

pub async fn void(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersMetadataVoidParams = parse(params, "papers.metadata.void")?;
    let library = parsed.library.clone();
    run_paper_metadata_write(
        ctx,
        library.as_deref(),
        "papers.metadata.void",
        move |ops| {
            let field = parsed.field.clone();
            let outcome = ops_papers::void_paper_metadata_field(
                ops,
                PaperVoidMetadataFieldRequest {
                    intake_id: parsed.intake_id,
                    field: parsed.field,
                    reason: parsed.reason,
                },
            )?;
            let mut body = outcome_json(&outcome);
            body["intake_id"] = json!(parsed.intake_id);
            body["field"] = json!(field);
            body["voided"] = json!(true);
            Ok(body)
        },
    )
    .await
}

// ─── ack / approve / reject / reopen ─────────────────────────────────

/// A review verb that may be left unexplained: `approve` records
/// agreement with what the pipeline already judged, `reopen` undoes an
/// earlier call.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PapersReviewParams {
    intake_id: i64,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    library: Option<String>,
}

/// A review verb that must be justified: `ack` waves a flagged record
/// through and `reject` takes one out of circulation, so the trail is
/// worth little without the words that go with it.
///
/// The requirement is a separate struct rather than a check in the
/// body, so a missing reason is refused where every other malformed
/// request is — at parse time, before the write path is entered.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PapersJustifiedReviewParams {
    intake_id: i64,
    reason: String,
    #[serde(default)]
    library: Option<String>,
}

impl From<PapersJustifiedReviewParams> for PapersReviewParams {
    fn from(p: PapersJustifiedReviewParams) -> PapersReviewParams {
        PapersReviewParams {
            intake_id: p.intake_id,
            reason: Some(p.reason),
            library: p.library,
        }
    }
}

/// One review verb: which op runs, and the status string the response
/// reports.
type ReviewOp = fn(
    &bookrack_ops::Ops<bookrack_embed::OllamaEmbedClient>,
    PaperReviewRequest,
) -> bookrack_ops::Result<bookrack_ops::dto::writes::WriteOutcome>;

async fn write_review_status(
    ctx: &MethodContext,
    parsed: PapersReviewParams,
    method: &'static str,
    status: &'static str,
    op: ReviewOp,
) -> Result<Value, RpcError> {
    let library = parsed.library.clone();
    run_paper_metadata_write(ctx, library.as_deref(), method, move |ops| {
        let outcome = op(
            ops,
            PaperReviewRequest {
                intake_id: parsed.intake_id,
                reason: parsed.reason,
            },
        )?;
        let mut body = outcome_json(&outcome);
        body["intake_id"] = json!(parsed.intake_id);
        body["status"] = json!(status);
        Ok(body)
    })
    .await
}

pub async fn ack(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    write_review_status(
        ctx,
        parse::<PapersJustifiedReviewParams>(params, "papers.metadata.ack")?.into(),
        "papers.metadata.ack",
        bookrack_catalog::STATUS_ACKNOWLEDGED,
        ops_papers::acknowledge_paper_metadata_gap,
    )
    .await
}

pub async fn approve(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    write_review_status(
        ctx,
        parse(params, "papers.metadata.approve")?,
        "papers.metadata.approve",
        bookrack_catalog::STATUS_APPROVED,
        ops_papers::approve_paper_metadata,
    )
    .await
}

pub async fn reject(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    write_review_status(
        ctx,
        parse::<PapersJustifiedReviewParams>(params, "papers.metadata.reject")?.into(),
        "papers.metadata.reject",
        bookrack_catalog::STATUS_REJECTED,
        ops_papers::reject_paper_metadata,
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
        "papers.metadata.reopen",
        bookrack_catalog::STATUS_PENDING,
        ops_papers::reopen_paper_review,
    )
    .await
}

// ─── contributor_add / contributor_remove ───────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    reason: Option<String>,
    #[serde(default)]
    library: Option<String>,
}

pub async fn contributor_add(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let parsed: PapersContributorAddParams = parse(params, "papers.metadata.contributor_add")?;
    let library = parsed.library.clone();
    run_paper_metadata_write(
        ctx,
        library.as_deref(),
        "papers.metadata.contributor_add",
        move |ops| {
            let role = parsed.role.clone();
            let name = parsed.name.clone();
            let outcome = ops_papers::add_paper_contributor(
                ops,
                PaperContributorAddRequest {
                    intake_id: parsed.intake_id,
                    role: parsed.role,
                    name: parsed.name,
                    family: parsed.family,
                    given: parsed.given,
                    orcid: parsed.orcid,
                    reason: parsed.reason,
                },
            )?;
            let mut body = outcome_json(&outcome.write);
            body["intake_id"] = json!(parsed.intake_id);
            body["contributor_id"] = json!(outcome.contributor_id);
            body["role"] = json!(role);
            body["name"] = json!(name);
            Ok(body)
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PapersContributorRemoveParams {
    intake_id: i64,
    contributor_id: i64,
    #[serde(default)]
    reason: Option<String>,
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
    run_paper_metadata_write(
        ctx,
        library.as_deref(),
        "papers.metadata.contributor_remove",
        move |ops| {
            let outcome = ops_papers::remove_paper_contributor(
                ops,
                PaperContributorRemoveRequest {
                    intake_id: parsed.intake_id,
                    contributor_id: parsed.contributor_id,
                    reason: parsed.reason,
                },
            )?;
            let mut body = outcome_json(&outcome);
            body["intake_id"] = json!(parsed.intake_id);
            body["contributor_id"] = json!(parsed.contributor_id);
            body["removed"] = json!(outcome.changed);
            Ok(body)
        },
    )
    .await
}
