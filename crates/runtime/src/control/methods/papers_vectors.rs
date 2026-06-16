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

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, require_yes, run_write};
use crate::cmd::papers_vectors;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

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
        .map_err(|e| {
            RpcError::new(
                INTERNAL_ERROR,
                format!("papers.vectors_rebuild failed: {e:#}"),
            )
        })?;
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
}

pub async fn reembed(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersVectorsReembedParams = parse(params, "papers.vectors_reembed")?;
    require_yes("papers.vectors_reembed", parsed.yes, parsed.dry_run)?;
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
        .map_err(|e| {
            RpcError::new(
                INTERNAL_ERROR,
                format!("papers.vectors_reembed failed: {e:#}"),
            )
        })?;
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
            .map_err(|e| {
                RpcError::new(
                    INTERNAL_ERROR,
                    format!("papers.vectors_reset failed: {e:#}"),
                )
            })?;
        Ok(json!({ "ok": true }))
    })
    .await
}

pub async fn drop_index(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        papers_vectors::drop(&cfg).await.map_err(|e| {
            RpcError::new(INTERNAL_ERROR, format!("papers.vectors_drop failed: {e:#}"))
        })?;
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
