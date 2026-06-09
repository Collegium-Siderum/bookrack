// SPDX-License-Identifier: Apache-2.0

//! `corpus.rebuild` JSON-RPC handler.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::corpus;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

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
            |_prompt| Ok(true),
        )
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("corpus.rebuild failed: {e:#}")))?;
        Ok(json!({ "ok": true }))
    })
    .await
}
