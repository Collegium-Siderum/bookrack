// SPDX-License-Identifier: Apache-2.0

//! `papers.remove` JSON-RPC handler. Paper-side peer of `remove`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::remove_paper::{
    RemovePaperArgs, execute_remove_from_plan, plan_remove, run as run_remove_paper,
};
use crate::control::error_map::{plan_lookup_err, write_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::control::plan_registry::PlanId;

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PapersRemoveParams {
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
    /// on execute; the legacy unpinned fallback fires when this is
    /// absent and logs a deprecation warning.
    #[serde(default)]
    plan_id: Option<String>,
}

/// Serialized form of a registered `papers.remove` plan.
#[derive(Debug, Serialize, Deserialize)]
struct RegisteredPapersRemovePlan {
    intake_id: i64,
}

pub async fn run(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersRemoveParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid papers.remove params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "missing papers.remove params",
            ));
        }
    };

    if parsed.dry_run {
        if parsed.intake_id.is_none() && parsed.sha.is_none() {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "papers.remove: pass intake_id or sha",
            ));
        }
        return remove_dry_run(parsed, ctx).await;
    }
    match parsed.plan_id.as_deref() {
        Some(id) => remove_execute_from_plan(id.to_string(), ctx).await,
        None => remove_legacy_execute(parsed, ctx).await,
    }
}

async fn remove_dry_run(
    parsed: PapersRemoveParams,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    let library_name = ctx.library_name.clone();
    let registry = ctx.plan_registry.clone();
    let args = RemovePaperArgs {
        intake_id: parsed.intake_id,
        sha: parsed.sha,
        dry_run: true,
        yes: parsed.yes,
    };
    run_write(ctx, move || async move {
        let plan = plan_remove(&cfg, &args)
            .await
            .map_err(|e| write_err("papers.remove", e))?;
        let registered = RegisteredPapersRemovePlan {
            intake_id: plan.intake.intake_id,
        };
        let plan_id = registry
            .register("papers.remove", library_name, &registered)
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
            "source_pdf_path": plan.source_pdf_path,
            "source_pdf_exists": plan.source_pdf_exists,
            "catalog_counts": serialize_counts(&plan.counts),
        }))
    })
    .await
}

async fn remove_execute_from_plan(plan_id: String, ctx: &MethodContext) -> Result<Value, RpcError> {
    let payload = ctx
        .plan_registry
        .take(
            &PlanId::from(plan_id),
            "papers.remove",
            ctx.library_name.as_str(),
        )
        .map_err(plan_lookup_err)?;
    let plan: RegisteredPapersRemovePlan = serde_json::from_slice(&payload)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("decode plan payload: {e}")))?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        let outcome = execute_remove_from_plan(&cfg, plan.intake_id)
            .await
            .map_err(|e| write_err("papers.remove", e))?;
        Ok(json!({
            "intake_id": outcome.intake_id,
            "source_sha256": outcome.source_sha256,
            "intake_row_existed": outcome.intake_row_existed,
            "catalog_deleted": serialize_counts(&outcome.catalog_deleted),
        }))
    })
    .await
}

async fn remove_legacy_execute(
    parsed: PapersRemoveParams,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    if parsed.intake_id.is_none() && parsed.sha.is_none() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "papers.remove: pass intake_id or sha",
        ));
    }
    tracing::warn!(
        "papers.remove: execute without plan_id; falling back to legacy unpinned path. \
         Clients should run dry_run=true first and present the returned plan_id."
    );
    let cfg = ctx.cfg.clone();
    let args = RemovePaperArgs {
        intake_id: parsed.intake_id,
        sha: parsed.sha,
        dry_run: parsed.dry_run,
        yes: parsed.yes,
    };
    run_write(ctx, move || async move {
        run_remove_paper(&cfg, args)
            .await
            .map_err(|e| write_err("papers.remove", e))?;
        Ok(json!({ "ok": true }))
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
