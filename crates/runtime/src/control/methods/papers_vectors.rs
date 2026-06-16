// SPDX-License-Identifier: Apache-2.0

//! `papers.vectors_{rebuild,reembed,reset,drop}` JSON-RPC handlers.
//!
//! Peer of [`super::vectors`] for the paper pipeline.
//!
//! As with the book-side vectors handlers, the control plane never
//! prompts on the caller's behalf: destructive methods that expose a
//! `yes` parameter reject requests with `yes = false` up front, and
//! the `ask` closure handed to the cmd layer denies any prompt that
//! does reach it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, require_yes, run_write};
use crate::cmd::papers_vectors;
use crate::control::error_map::{plan_lookup_err, write_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::control::plan_registry::PlanId;

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PapersVectorsRebuildParams {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    num_partitions: Option<u32>,
    #[serde(default)]
    num_sub_vectors: Option<u32>,
    #[serde(default)]
    num_bits: Option<u32>,
    #[serde(default)]
    nprobes: Option<u32>,
    #[serde(default)]
    refine_factor: Option<u32>,
}

pub async fn rebuild(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersVectorsRebuildParams = parse(params, "papers.vectors_rebuild")?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        papers_vectors::rebuild(
            &cfg,
            parsed.kind.as_deref(),
            parsed.num_partitions,
            parsed.num_sub_vectors,
            parsed.num_bits,
            parsed.nprobes,
            parsed.refine_factor,
        )
        .await
        .map_err(|e| write_err("papers.vectors_rebuild", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PapersVectorsReembedParams {
    #[serde(default)]
    paper: Option<i64>,
    #[serde(default)]
    stale_only: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    yes: bool,
    /// Returned by the dry-run leg and presented by the execute leg
    /// to commit the exact plan the operator confirmed. Required on
    /// execute; the legacy unpinned fallback fires when this is
    /// absent and logs a deprecation warning.
    #[serde(default)]
    plan_id: Option<String>,
}

/// Serialized form of a registered `papers.vectors_reembed` plan.
#[derive(Debug, Serialize, Deserialize)]
struct RegisteredPapersReembedPlan {
    pinned_ids: Vec<i64>,
}

pub async fn reembed(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersVectorsReembedParams = parse(params, "papers.vectors_reembed")?;
    require_yes("papers.vectors_reembed", parsed.yes, parsed.dry_run)?;

    if parsed.dry_run {
        return reembed_dry_run(parsed, ctx).await;
    }
    match parsed.plan_id.as_deref() {
        Some(id) => reembed_execute_from_plan(id.to_string(), ctx).await,
        None => reembed_legacy_execute(parsed, ctx).await,
    }
}

async fn reembed_dry_run(
    parsed: PapersVectorsReembedParams,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    let library_name = ctx.library_name.clone();
    let registry = ctx.plan_registry.clone();
    let paper = parsed.paper;
    let stale_only = parsed.stale_only;
    run_write(ctx, move || async move {
        let plans = papers_vectors::plan_reembed(&cfg, paper, stale_only)
            .await
            .map_err(|e| write_err("papers.vectors_reembed", e))?;
        let registered = RegisteredPapersReembedPlan {
            pinned_ids: plans.iter().map(|p| p.intake_id).collect(),
        };
        let plan_id = registry
            .register("papers.vectors_reembed", library_name, &registered)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("register plan: {e}")))?;
        Ok(json!({
            "plan_id": plan_id.as_str(),
            "plans": plans
                .iter()
                .map(|p| json!({
                    "intake_id": p.intake_id,
                    "partition": p.partition.get(),
                    "chunk_count": p.chunk_count,
                    "total_chars": p.total_chars,
                }))
                .collect::<Vec<_>>(),
        }))
    })
    .await
}

async fn reembed_execute_from_plan(
    plan_id: String,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let payload = ctx
        .plan_registry
        .take(
            &PlanId::from(plan_id),
            "papers.vectors_reembed",
            ctx.library_name.as_str(),
        )
        .map_err(plan_lookup_err)?;
    let plan: RegisteredPapersReembedPlan = serde_json::from_slice(&payload)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("decode plan payload: {e}")))?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        let report = papers_vectors::execute_reembed_from_plan(&cfg, plan.pinned_ids)
            .await
            .map_err(|e| write_err("papers.vectors_reembed", e))?;
        let chunks_written: usize = report.intakes.iter().map(|o| o.chunks_written).sum();
        Ok(json!({
            "reembedded_intakes": report.intakes.len(),
            "reembedded_chunks": chunks_written,
            "skipped_empty": report.skipped_empty,
        }))
    })
    .await
}

async fn reembed_legacy_execute(
    parsed: PapersVectorsReembedParams,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    tracing::warn!(
        "papers.vectors_reembed: execute without plan_id; falling back to legacy unpinned path. \
         Clients should run dry_run=true first and present the returned plan_id."
    );
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        papers_vectors::reembed(
            &cfg,
            parsed.paper,
            parsed.stale_only,
            parsed.dry_run,
            parsed.yes,
            deny_destructive,
        )
        .await
        .map_err(|e| write_err("papers.vectors_reembed", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PapersVectorsResetParams {
    #[serde(default)]
    yes: bool,
    #[serde(default)]
    resume: bool,
}

pub async fn reset(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersVectorsResetParams = parse(params, "papers.vectors_reset")?;
    require_yes("papers.vectors_reset", parsed.yes, parsed.resume)?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        papers_vectors::reset(&cfg, parsed.yes, parsed.resume, deny_destructive)
            .await
            .map_err(|e| write_err("papers.vectors_reset", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

pub async fn drop_index(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        papers_vectors::drop(&cfg)
            .await
            .map_err(|e| write_err("papers.vectors_drop", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

fn parse<T: serde::de::DeserializeOwned + Default>(
    params: &Option<Value>,
    method: &str,
) -> Result<T, RpcError> {
    match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid {method} params: {e}"))),
        _ => Ok(T::default()),
    }
}

fn deny_destructive(_prompt: &str) -> anyhow::Result<bool> {
    Ok(false)
}
