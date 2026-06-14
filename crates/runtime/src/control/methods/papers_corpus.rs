// SPDX-License-Identifier: Apache-2.0

//! `papers.corpus_rebuild` JSON-RPC handler.
//!
//! Peer of [`super::corpus::rebuild`] for the paper pipeline.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
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
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        papers_corpus::rebuild(
            &cfg,
            parsed.include_vectors,
            parsed.paper,
            parsed.stale_only,
            parsed.dry_run,
            parsed.yes,
            |_prompt| Ok(true),
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
