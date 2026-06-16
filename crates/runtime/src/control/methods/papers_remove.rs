// SPDX-License-Identifier: Apache-2.0

//! `papers.remove` JSON-RPC handler. Paper-side peer of `remove`.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::remove_paper::{RemovePaperArgs, run as run_remove_paper};
use crate::control::error_map::write_err;
use crate::control::jsonrpc::{INVALID_PARAMS, RpcError};

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PapersRemoveParams {
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
    let parsed: PapersRemoveParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid papers.remove params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "missing papers.remove params",
            ));
        }
    };
    if parsed.intake_id.is_none() && parsed.sha.is_none() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "papers.remove: pass intake_id or sha",
        ));
    }
    let cfg = ctx.cfg.clone();
    let args = RemovePaperArgs {
        intake_id: parsed.intake_id,
        sha: parsed.sha,
        dry_run: parsed.dry_run,
        yes: parsed.yes,
    };
    run_write(ctx, move || async move {
        run_remove_paper(&cfg, args)
            .await
            .map_err(|e| write_err("papers.remove", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}
