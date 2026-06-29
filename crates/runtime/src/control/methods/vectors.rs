// SPDX-License-Identifier: Apache-2.0

//! `vectors.{rebuild,reembed,reset,drop}` JSON-RPC handlers.
//!
//! Mirrors the CLI subcommand surface, plumbing each through
//! [`super::run_write`]. The control plane never prompts on the
//! caller's behalf: any destructive method that exposes a `yes`
//! parameter rejects requests with `yes = false` up front, and the
//! `ask` closure handed to the cmd layer denies any prompt that
//! does reach it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, require_yes, run_write};
use crate::cmd::vectors;
use crate::control::error_map::{plan_lookup_err, write_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::control::plan_registry::PlanId;

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct VectorsRebuildParams {
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
    let parsed: VectorsRebuildParams = parse(params, "vectors.rebuild")?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        vectors::rebuild(
            &cfg,
            parsed.kind.as_deref(),
            parsed.num_partitions,
            parsed.num_sub_vectors,
            parsed.num_bits,
            parsed.nprobes,
            parsed.refine_factor,
        )
        .await
        .map_err(|e| write_err("vectors.rebuild", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct VectorsReembedParams {
    #[serde(default)]
    book: Option<i64>,
    #[serde(default)]
    stale_only: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    yes: bool,
    /// Returned by the dry-run leg and presented by the execute leg
    /// to commit the exact plan the operator confirmed. Required on
    /// execute; the call returns INVALID_PARAMS when this is absent.
    #[serde(default)]
    plan_id: Option<String>,
}

/// Serialized form of a registered `vectors.reembed` plan. The
/// pinned set is the dry-run plan's intake-id sequence.
#[derive(Debug, Serialize, Deserialize)]
struct RegisteredReembedPlan {
    pinned_ids: Vec<i64>,
}

pub async fn reembed(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: VectorsReembedParams = parse(params, "vectors.reembed")?;
    require_yes("vectors.reembed", parsed.yes, parsed.dry_run)?;

    if parsed.dry_run {
        return reembed_dry_run(parsed, ctx).await;
    }
    match parsed.plan_id.as_deref() {
        Some(id) => reembed_execute_from_plan(id.to_string(), ctx).await,
        None => Err(RpcError::new(
            INVALID_PARAMS,
            "vectors.reembed: plan_id required on execute; call with dry_run=true first \
             and present the returned plan_id",
        )),
    }
}

async fn reembed_dry_run(
    parsed: VectorsReembedParams,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    let library_name = ctx.library_name.clone();
    let registry = ctx.plan_registry.clone();
    let book = parsed.book;
    let stale_only = parsed.stale_only;
    run_write(ctx, move || async move {
        let plans = vectors::plan_reembed(&cfg, book, stale_only)
            .await
            .map_err(|e| write_err("vectors.reembed", e))?;
        let registered = RegisteredReembedPlan {
            pinned_ids: plans.iter().map(|p| p.intake_id).collect(),
        };
        let plan_id = registry
            .register("vectors.reembed", library_name, &registered)
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
            "vectors.reembed",
            ctx.library_name.as_str(),
        )
        .map_err(plan_lookup_err)?;
    let plan: RegisteredReembedPlan = serde_json::from_slice(&payload)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("decode plan payload: {e}")))?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        let report = vectors::execute_reembed_from_plan(&cfg, plan.pinned_ids)
            .await
            .map_err(|e| write_err("vectors.reembed", e))?;
        let chunks_written: usize = report
            .intakes
            .iter()
            .map(|o| o.embed_run.chunks_written)
            .sum();
        Ok(json!({
            "reembedded_intakes": report.intakes.len(),
            "reembedded_chunks": chunks_written,
            "skipped_empty": report.skipped_empty,
        }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct VectorsResetParams {
    #[serde(default)]
    yes: bool,
    #[serde(default)]
    resume: bool,
}

pub async fn reset(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: VectorsResetParams = parse(params, "vectors.reset")?;
    require_yes("vectors.reset", parsed.yes, parsed.resume)?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        vectors::reset(&cfg, parsed.yes, parsed.resume, deny_destructive)
            .await
            .map_err(|e| write_err("vectors.reset", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct VectorsDropParams {
    #[serde(default)]
    yes: bool,
}

pub async fn drop_index(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: VectorsDropParams = parse(params, "vectors.drop")?;
    require_yes("vectors.drop", parsed.yes, false)?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        vectors::drop(&cfg)
            .await
            .map_err(|e| write_err("vectors.drop", e))?;
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

fn deny_destructive(_prompt: &str) -> eyre::Result<bool> {
    Ok(false)
}
