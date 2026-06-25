// SPDX-License-Identifier: Apache-2.0

//! `remove` JSON-RPC handler.
//!
//! Drives the two-step pinned protocol for book removal: dry_run
//! computes the plan and registers it; execute presents the
//! returned plan_id and removes the intake that was confirmed,
//! independent of any catalog drift between the two RPCs.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::remove::{ExpectedFingerprint, RemoveArgs, execute_remove_from_plan, plan_remove};
use crate::control::error_map::{plan_lookup_err, write_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::control::plan_registry::PlanId;

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct RemoveParams {
    #[serde(default)]
    intake_id: Option<i64>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    yes: bool,
    /// Returned by the dry-run leg and presented by the execute leg
    /// to commit the exact intake the operator confirmed. Required
    /// on execute; the call returns INVALID_PARAMS when this is absent.
    #[serde(default)]
    plan_id: Option<String>,
}

/// Serialized form of a registered `remove` plan. The `fingerprint`
/// pins the targeted state observed at dry-run time so the execute
/// leg can reject a drifted target instead of deleting under an
/// unconfirmed payload.
#[derive(Debug, Serialize, Deserialize)]
struct RegisteredRemovePlan {
    intake_id: i64,
    fingerprint: String,
}

pub async fn run(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: RemoveParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid remove params: {e}")))?,
        _ => return Err(RpcError::new(INVALID_PARAMS, "missing remove params")),
    };

    if parsed.dry_run {
        if parsed.intake_id.is_none() && parsed.sha.is_none() {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "remove: pass intake_id or sha",
            ));
        }
        return remove_dry_run(parsed, ctx).await;
    }
    match parsed.plan_id.as_deref() {
        Some(id) => remove_execute_from_plan(id.to_string(), ctx).await,
        None => Err(RpcError::new(
            INVALID_PARAMS,
            "remove: plan_id required on execute; call with dry_run=true first and \
             present the returned plan_id",
        )),
    }
}

async fn remove_dry_run(parsed: RemoveParams, ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    let library_name = ctx.library_name.clone();
    let registry = ctx.plan_registry.clone();
    let args = RemoveArgs {
        intake_id: parsed.intake_id,
        sha: parsed.sha,
        dry_run: true,
        yes: parsed.yes,
    };
    run_write(ctx, move || async move {
        let plan = plan_remove(&cfg, &args)
            .await
            .map_err(|e| write_err("remove", e))?;
        let fingerprint = plan.fingerprint();
        let registered = RegisteredRemovePlan {
            intake_id: plan.intake.intake_id,
            fingerprint,
        };
        let plan_id = registry
            .register("remove", library_name, &registered)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("register plan: {e}")))?;
        Ok(json!({
            "plan_id": plan_id.as_str(),
            "intake_id": plan.intake.intake_id,
            "source_sha256": plan.intake.source_sha256,
            "status": plan.intake.status.as_str(),
            "corpus_nodes": plan.corpus_nodes,
            "vector_rows": plan.vector_rows,
            "envelope_path": plan.envelope_path,
            "envelope_exists": plan.envelope_exists,
            "catalog_counts": serialize_counts(&plan.counts),
        }))
    })
    .await
}

async fn remove_execute_from_plan(plan_id: String, ctx: &MethodContext) -> Result<Value, RpcError> {
    let payload = ctx
        .plan_registry
        .take(&PlanId::from(plan_id), "remove", ctx.library_name.as_str())
        .map_err(plan_lookup_err)?;
    let plan: RegisteredRemovePlan = serde_json::from_slice(&payload)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("decode plan payload: {e}")))?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        let outcome = execute_remove_from_plan(
            &cfg,
            plan.intake_id,
            ExpectedFingerprint::Required(&plan.fingerprint),
        )
        .await
        .map_err(|e| write_err("remove", e))?;
        Ok(json!({
            "intake_id": outcome.intake_id,
            "source_sha256": outcome.source_sha256,
            "intake_row_existed": outcome.intake_row_existed,
            "catalog_deleted": serialize_counts(&outcome.catalog_deleted),
        }))
    })
    .await
}

fn serialize_counts(c: &bookrack_catalog::ItemRemovalCounts) -> Value {
    json!({
        "total": c.total(),
        "book_state": c.book_state,
        "publication_attrs": c.node_publication_attrs,
        "overrides": c.node_overrides,
        "contributors": c.node_contributors,
        "categories": c.node_categories,
        "reviews": c.node_reviews,
        "role_takeovers": c.node_role_takeovers,
        "toc_edits": c.toc_edits,
    })
}
