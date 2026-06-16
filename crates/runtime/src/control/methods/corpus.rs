// SPDX-License-Identifier: Apache-2.0

//! `corpus.rebuild` JSON-RPC handler.
//!
//! The control plane never prompts on the caller's behalf: a
//! destructive rebuild requires the client to pass `yes = true`.
//! Otherwise the handler short-circuits with [`CONFIRMATION_REQUIRED`]
//! and the supplied `ask` closure denies any prompt that does reach
//! the cmd layer.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, require_yes, run_write};
use crate::cmd::corpus;
use crate::control::error_map::write_err;
use crate::control::jsonrpc::{INVALID_PARAMS, RpcError};

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
    require_yes("corpus.rebuild", parsed.yes, parsed.dry_run)?;
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

fn deny_destructive(_prompt: &str) -> anyhow::Result<bool> {
    Ok(false)
}
