// SPDX-License-Identifier: Apache-2.0

//! `papers.corpus_rebuild` JSON-RPC handler.
//!
//! Peer of [`super::corpus::rebuild`] for the paper pipeline.
//!
//! As with `corpus.rebuild`, a destructive rebuild requires the
//! client to pass `yes = true`; the server never prompts on the
//! caller's behalf and the `ask` closure denies any prompt that
//! does reach the cmd layer.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, require_yes, run_write};
use crate::cmd::papers_corpus;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

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
        .map_err(|e| {
            RpcError::new(
                INTERNAL_ERROR,
                format!("papers.corpus_rebuild failed: {e:#}"),
            )
        })?;
        Ok(json!({ "ok": true }))
    })
    .await
}

fn deny_destructive(_prompt: &str) -> anyhow::Result<bool> {
    Ok(false)
}
