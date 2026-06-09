// SPDX-License-Identifier: Apache-2.0

//! `remove` JSON-RPC handler.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::remove::{RemoveArgs, run as run_remove};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct RemoveParams {
    #[serde(default)]
    intake_id: Option<i64>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    yes: bool,
}

pub async fn run(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: RemoveParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid remove params: {e}")))?,
        _ => return Err(RpcError::new(INVALID_PARAMS, "missing remove params")),
    };
    if parsed.intake_id.is_none() && parsed.sha.is_none() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "remove: pass intake_id or sha",
        ));
    }
    let cfg = ctx.cfg.clone();
    let args = RemoveArgs {
        intake_id: parsed.intake_id,
        sha: parsed.sha,
        dry_run: parsed.dry_run,
        yes: parsed.yes,
    };
    run_write(ctx, move || async move {
        run_remove(&cfg, args)
            .await
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("remove failed: {e:#}")))?;
        Ok(json!({ "ok": true }))
    })
    .await
}
