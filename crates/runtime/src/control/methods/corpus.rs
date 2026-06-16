// SPDX-License-Identifier: Apache-2.0

//! `corpus.rebuild` JSON-RPC handler.
//!
//! The control plane never prompts on the caller's behalf. A
//! destructive rebuild runs as two RPCs: the first carries
//! `dry_run = true`, the daemon computes the plan, registers it
//! under a freshly minted `plan_id`, and returns the id together
//! with the plan classification; the second carries `yes = true`
//! and `plan_id = <id>`, the daemon consumes the registered plan,
//! and the execute leg acts on the exact pinned target set —
//! independent of any catalog drift between the two RPCs.
//!
//! A transitional fallback path accepts `yes = true` with no
//! `plan_id` and falls back to the legacy unpinned execute. It is
//! present so existing clients keep working while the CLI migrates
//! to the two-step protocol; the warning logged on every such call
//! is the migration signal.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, require_yes, run_write};
use crate::cmd::corpus;
use crate::control::error_map::{plan_lookup_err, write_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::control::plan_registry::PlanId;

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct CorpusRebuildParams {
    #[serde(default)]
    include_vectors: bool,
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
    /// execute; the legacy unpinned fallback fires when this is
    /// absent and logs a deprecation warning.
    #[serde(default)]
    plan_id: Option<String>,
}

/// Serialized form of a registered `corpus.rebuild` plan. The
/// pinned set is the dry-run report's `rebuilt` bucket; the
/// include_vectors flag is captured because it shapes the execute
/// leg's sidecar work (stamp vs reembed) and would otherwise need
/// the client to re-send the original params verbatim.
#[derive(Debug, Serialize, Deserialize)]
struct RegisteredRebuildPlan {
    pinned_ids: Vec<i64>,
    include_vectors: bool,
}

pub async fn rebuild(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: CorpusRebuildParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                format!("invalid corpus.rebuild params: {e}"),
            )
        })?,
        _ => CorpusRebuildParams::default(),
    };
    require_yes("corpus.rebuild", parsed.yes, parsed.dry_run)?;

    if parsed.dry_run {
        return run_dry_run(parsed, ctx).await;
    }
    match parsed.plan_id.as_deref() {
        Some(id) => run_execute_from_plan(id.to_string(), ctx).await,
        None => run_legacy_execute(parsed, ctx).await,
    }
}

async fn run_dry_run(parsed: CorpusRebuildParams, ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    let library_name = ctx.library_name.clone();
    let registry = ctx.plan_registry.clone();
    let include_vectors = parsed.include_vectors;
    let book = parsed.book;
    let stale_only = parsed.stale_only;
    run_write(ctx, move || async move {
        let report = corpus::plan_rebuild(&cfg, book, stale_only)
            .map_err(|e| write_err("corpus.rebuild", e))?;
        let registered = RegisteredRebuildPlan {
            pinned_ids: report.rebuilt.clone(),
            include_vectors,
        };
        let plan_id = registry
            .register("corpus.rebuild", library_name, &registered)
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
            "corpus.rebuild",
            ctx.library_name.as_str(),
        )
        .map_err(plan_lookup_err)?;
    let plan: RegisteredRebuildPlan = serde_json::from_slice(&payload)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("decode plan payload: {e}")))?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        let outcome =
            corpus::execute_rebuild_from_plan(&cfg, plan.pinned_ids, plan.include_vectors)
                .await
                .map_err(|e| write_err("corpus.rebuild", e))?;
        Ok(serialize_execute_outcome(&outcome))
    })
    .await
}

async fn run_legacy_execute(
    parsed: CorpusRebuildParams,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    tracing::warn!(
        "corpus.rebuild: execute without plan_id; falling back to legacy unpinned path. \
         Clients should run dry_run=true first and present the returned plan_id."
    );
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        corpus::rebuild(
            &cfg,
            parsed.include_vectors,
            parsed.book,
            parsed.stale_only,
            parsed.dry_run,
            parsed.yes,
            None,
            deny_destructive,
        )
        .await
        .map_err(|e| write_err("corpus.rebuild", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

fn serialize_execute_outcome(o: &corpus::ExecuteRebuildOutcome) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_round_trip_with_plan_id() {
        let parsed: CorpusRebuildParams = serde_json::from_value(json!({
            "include_vectors": true,
            "book": 7,
            "stale_only": true,
            "yes": true,
            "plan_id": "plan_abc",
        }))
        .unwrap();
        assert!(parsed.include_vectors);
        assert_eq!(parsed.book, Some(7));
        assert!(parsed.stale_only);
        assert!(parsed.yes);
        assert!(!parsed.dry_run);
        assert_eq!(parsed.plan_id.as_deref(), Some("plan_abc"));
    }

    #[test]
    fn params_default_to_dry_run_false_and_plan_id_none() {
        let parsed: CorpusRebuildParams = serde_json::from_value(json!({})).unwrap();
        assert!(!parsed.dry_run);
        assert!(parsed.plan_id.is_none());
    }

    #[test]
    fn registered_plan_serde_round_trip() {
        let p = RegisteredRebuildPlan {
            pinned_ids: vec![1, 2, 3],
            include_vectors: true,
        };
        let bytes = serde_json::to_vec(&p).unwrap();
        let back: RegisteredRebuildPlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.pinned_ids, vec![1, 2, 3]);
        assert!(back.include_vectors);
    }
}
