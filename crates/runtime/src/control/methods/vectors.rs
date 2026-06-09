// SPDX-License-Identifier: Apache-2.0

//! `vectors.{rebuild,reembed,reset,drop}` JSON-RPC handlers.
//!
//! Mirrors the CLI subcommand surface, plumbing each through
//! [`super::run_write`]. The control-plane caller is assumed to have
//! confirmed any destructive prompt on their side, so the `ask`
//! closure simply approves.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::vectors;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

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
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("vectors.rebuild failed: {e:#}")))?;
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
}

pub async fn reembed(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: VectorsReembedParams = parse(params, "vectors.reembed")?;
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        vectors::reembed(
            &cfg,
            parsed.book,
            parsed.stale_only,
            parsed.dry_run,
            parsed.yes,
            None,
            approve_destructive,
        )
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("vectors.reembed failed: {e:#}")))?;
        Ok(json!({ "ok": true }))
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
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        vectors::reset(&cfg, parsed.yes, parsed.resume, approve_destructive)
            .await
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("vectors.reset failed: {e:#}")))?;
        Ok(json!({ "ok": true }))
    })
    .await
}

pub async fn drop_index(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        vectors::drop(&cfg)
            .await
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("vectors.drop failed: {e:#}")))?;
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

fn approve_destructive(_prompt: &str) -> anyhow::Result<bool> {
    Ok(true)
}
