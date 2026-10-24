// SPDX-License-Identifier: Apache-2.0

//! `vectors.{rebuild,reembed,reset,drop}` JSON-RPC handlers.
//!
//! Mirrors the CLI subcommand surface, plumbing each through
//! [`super::run_write`]. The control plane never prompts on the
//! caller's behalf: any destructive method that exposes a `yes`
//! parameter rejects requests with `yes = false` up front. Each write
//! answers with the report its command returned.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{MethodContext, ok_reply, require_yes, run_dry_run, run_write};
use crate::cmd::{ResetInterrupted, vectors};
use crate::control::error_map::{handler_err, plan_lookup_err, registry_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::control::plan_registry::PlanId;

#[derive(Debug, Default, Deserialize)]
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
    /// The library this call acts on. Absent means the registry's
    /// current default, the pointer `library.set_default` moves at run
    /// time.
    #[serde(default)]
    library: Option<String>,
}

pub async fn rebuild(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: VectorsRebuildParams = parse(params, "vectors.rebuild")?;
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    run_write(ctx, handle.name(), move || async move {
        let report = vectors::rebuild(
            &cfg,
            parsed.kind.as_deref(),
            parsed.num_partitions,
            parsed.num_sub_vectors,
            parsed.num_bits,
            parsed.nprobes,
            parsed.refine_factor,
        )
        .await
        .map_err(|e| handler_err("vectors.rebuild", e))?;
        ok_reply(&report)
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
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
    /// The library this call acts on. Absent means the registry's
    /// current default, the pointer `library.set_default` moves at run
    /// time.
    #[serde(default)]
    library: Option<String>,
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
        Some(id) => reembed_execute_from_plan(id.to_string(), parsed.library.as_deref(), ctx).await,
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
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    let library_name = handle.name().to_string();
    let registry = ctx.plan_registry.clone();
    let book = parsed.book;
    let stale_only = parsed.stale_only;
    run_dry_run(ctx, handle.name(), move || async move {
        let plans = vectors::plan_reembed(&cfg, book, stale_only)
            .await
            .map_err(|e| handler_err("vectors.reembed", e))?;
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
    library: Option<&str>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    // Resolve the target before the plan: the plan registry scopes an
    // id to the library it was minted against, and that comparison is
    // only meaningful against the library this call names.
    let handle = ctx.registry.get(library).map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    let payload = ctx
        .plan_registry
        .take(&PlanId::from(plan_id), "vectors.reembed", handle.name())
        .map_err(plan_lookup_err)?;
    let plan: RegisteredReembedPlan = serde_json::from_slice(&payload)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("decode plan payload: {e}")))?;
    run_write(ctx, handle.name(), move || async move {
        let report = vectors::execute_reembed_from_plan(&cfg, plan.pinned_ids)
            .await
            .map_err(|e| handler_err("vectors.reembed", e))?;
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
pub struct VectorsResetParams {
    #[serde(default)]
    yes: bool,
    #[serde(default)]
    resume: bool,
    /// The library this call acts on. Absent means the registry's
    /// current default, the pointer `library.set_default` moves at run
    /// time.
    #[serde(default)]
    library: Option<String>,
}

pub async fn reset(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: VectorsResetParams = parse(params, "vectors.reset")?;
    require_yes("vectors.reset", parsed.yes, parsed.resume)?;
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    run_write(ctx, handle.name(), move || async move {
        let report = vectors::reset(&cfg, parsed.resume)
            .await
            .map_err(|e| reset_err("vectors.reset", e))?;
        ok_reply(&report)
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
pub struct VectorsDropParams {
    #[serde(default)]
    yes: bool,
    /// The library this call acts on. Absent means the registry's
    /// current default, the pointer `library.set_default` moves at run
    /// time.
    #[serde(default)]
    library: Option<String>,
}

pub async fn drop_index(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: VectorsDropParams = parse(params, "vectors.drop")?;
    require_yes("vectors.drop", parsed.yes, false)?;
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    run_write(ctx, handle.name(), move || async move {
        let report = vectors::drop(&cfg)
            .await
            .map_err(|e| handler_err("vectors.drop", e))?;
        ok_reply(&report)
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

/// The step a reset that stopped partway leaves the operator.
const RESUME_STEP: &str =
    "Once the cause is addressed, rerun the reset with `--resume` to continue where it stopped.";

/// Map a reset failure like any other handler error, then, when the
/// reset pass itself failed, add the resumed rerun to the hint. A
/// failure while setting the reset up changed nothing, so a resume
/// would skip a reset that never ran; those keep the hint they had.
pub(super) fn reset_err(method: &str, e: eyre::Report) -> RpcError {
    let resumable = e.downcast_ref::<ResetInterrupted>().is_some();
    let mut err = handler_err(method, e);
    if resumable {
        err.append_hint(RESUME_STEP);
    }
    err
}

routed_params!(
    VectorsRebuildParams,
    VectorsReembedParams,
    VectorsResetParams,
    VectorsDropParams,
);

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_embed::EmbedError;
    use eyre::WrapErr;

    fn overloaded() -> EmbedError {
        EmbedError::Overloaded {
            status: 503,
            body: "busy".to_string(),
        }
    }

    /// The marker is context on the chain, not a type the classifier
    /// knows: the envelope keeps the code, summary, detail, and
    /// retryable the leaf maps to, and the resume step follows the
    /// leaf's own hint.
    #[test]
    fn a_failure_of_the_reset_pass_keeps_its_classification_and_gains_the_resume_step() {
        let bare = handler_err("vectors.reset", eyre::Report::new(overloaded()));
        let marked = Err::<(), _>(overloaded())
            .wrap_err(ResetInterrupted)
            .unwrap_err();
        assert!(
            marked.downcast_ref::<ResetInterrupted>().is_some(),
            "the marker must be recoverable from the report"
        );

        let err = reset_err("vectors.reset", marked);

        assert_eq!(err.code, bare.code);
        assert_eq!(err.message, bare.message);
        let (data, bare_data) = (
            err.data.expect("data"),
            bare.data.expect("the leaf maps to a data object"),
        );
        assert_eq!(data["detail"], bare_data["detail"]);
        assert_eq!(data["retryable"], bare_data["retryable"]);
        let leaf_hint = bare_data["hint"].as_str().expect("the leaf writes a hint");
        assert_eq!(
            data["hint"].as_str(),
            Some(format!("{leaf_hint} {RESUME_STEP}").as_str())
        );
    }

    /// A failure before the reset pass began changed nothing, so its
    /// envelope is exactly what the classifier made of it.
    #[test]
    fn a_failure_ahead_of_the_reset_pass_is_left_as_classified() {
        let bare = handler_err("vectors.reset", eyre::Report::new(overloaded()));
        let err = reset_err("vectors.reset", eyre::Report::new(overloaded()));
        assert_eq!(err.code, bare.code);
        assert_eq!(err.message, bare.message);
        assert_eq!(err.data, bare.data);
    }
}
