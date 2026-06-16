// SPDX-License-Identifier: Apache-2.0

//! `papers.corpus_rebuild` JSON-RPC handler.
//!
//! Peer of [`super::corpus::rebuild`] for the paper pipeline: same
//! two-step pinned protocol, with a transitional fallback for
//! execute-without-plan_id so existing clients keep working until
//! the CLI migrates.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, require_yes, run_write};
use crate::cmd::papers_corpus;
use crate::control::error_map::{plan_lookup_err, write_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::control::plan_registry::PlanId;

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PapersCorpusRebuildParams {
    #[serde(default)]
    include_vectors: bool,
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

/// Serialized form of a registered `papers.corpus_rebuild` plan.
#[derive(Debug, Serialize, Deserialize)]
struct RegisteredPapersRebuildPlan {
    pinned_ids: Vec<i64>,
    include_vectors: bool,
}

pub async fn rebuild(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersCorpusRebuildParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                format!("invalid papers.corpus_rebuild params: {e}"),
            )
        })?,
        _ => PapersCorpusRebuildParams::default(),
    };
    require_yes("papers.corpus_rebuild", parsed.yes, parsed.dry_run)?;

    if parsed.dry_run {
        return run_dry_run(parsed, ctx).await;
    }
    match parsed.plan_id.as_deref() {
        Some(id) => run_execute_from_plan(id.to_string(), ctx).await,
        None => run_legacy_execute(parsed, ctx).await,
    }
}

async fn run_dry_run(
    parsed: PapersCorpusRebuildParams,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    let library_name = ctx.library_name.clone();
    let registry = ctx.plan_registry.clone();
    let include_vectors = parsed.include_vectors;
    let paper = parsed.paper;
    let stale_only = parsed.stale_only;
    run_write(ctx, move || async move {
        let report = papers_corpus::plan_rebuild(&cfg, paper, stale_only)
            .map_err(|e| write_err("papers.corpus_rebuild", e))?;
        let registered = RegisteredPapersRebuildPlan {
            pinned_ids: report.rebuilt.clone(),
            include_vectors,
        };
        let plan_id = registry
            .register("papers.corpus_rebuild", library_name, &registered)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("register plan: {e}")))?;
        Ok(json!({
            "plan_id": plan_id.as_str(),
            "rebuilt": report.rebuilt,
            "missing_envelope": report.missing_envelope,
            "mismatched": report.mismatched,
            "failed": report
                .failed
                .iter()
                .map(|(id, err)| json!({ "intake_id": id, "error": err }))
                .collect::<Vec<_>>(),
        }))
    })
    .await
}

async fn run_execute_from_plan(plan_id: String, ctx: &MethodContext) -> Result<Value, RpcError> {
    let payload = ctx
        .plan_registry
        .take(
            &PlanId::from(plan_id),
            "papers.corpus_rebuild",
            ctx.library_name.as_str(),
        )
        .map_err(plan_lookup_err)?;
    let plan: RegisteredPapersRebuildPlan = serde_json::from_slice(&payload)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("decode plan payload: {e}")))?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        let outcome =
            papers_corpus::execute_rebuild_from_plan(&cfg, plan.pinned_ids, plan.include_vectors)
                .await
                .map_err(|e| write_err("papers.corpus_rebuild", e))?;
        Ok(serialize_execute_outcome(&outcome))
    })
    .await
}

async fn run_legacy_execute(
    parsed: PapersCorpusRebuildParams,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    tracing::warn!(
        "papers.corpus_rebuild: execute without plan_id; falling back to legacy unpinned path. \
         Clients should run dry_run=true first and present the returned plan_id."
    );
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        papers_corpus::rebuild(
            &cfg,
            parsed.include_vectors,
            parsed.paper,
            parsed.stale_only,
            parsed.dry_run,
            parsed.yes,
            deny_destructive,
        )
        .await
        .map_err(|e| write_err("papers.corpus_rebuild", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

fn serialize_execute_outcome(o: &papers_corpus::ExecutePapersRebuildOutcome) -> Value {
    let mut v = json!({
        "rebuilt": o.report.rebuilt,
        "missing_envelope": o.report.missing_envelope,
        "mismatched": o.report.mismatched,
        "failed": o.report
            .failed
            .iter()
            .map(|(id, err)| json!({ "intake_id": id, "error": err }))
            .collect::<Vec<_>>(),
    });
    if let Some(stamped) = o.stamped_from_existing_chunks {
        v["stamped_from_existing_chunks"] = json!(stamped);
    }
    if let Some(re) = &o.reembed {
        v["reembedded_intakes"] = json!(re.intakes);
        v["reembedded_chunks"] = json!(re.chunks_written);
    }
    v
}

fn deny_destructive(_prompt: &str) -> anyhow::Result<bool> {
    Ok(false)
}
