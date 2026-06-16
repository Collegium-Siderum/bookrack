// SPDX-License-Identifier: Apache-2.0

//! `dryrun` JSON-RPC handler. Walks a path and runs the pre-vector
//! simulation, writing the JSONL plus a summary sidecar under
//! `<data_root>/dryruns/`.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::dryrun;
use crate::control::error_map::write_err;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct DryrunParams {
    #[cfg_attr(test, ts(type = "string"))]
    path: PathBuf,
    #[serde(default)]
    #[cfg_attr(test, ts(type = "string | null"))]
    out: Option<PathBuf>,
    #[serde(default)]
    no_chunk: bool,
}

pub async fn run(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: DryrunParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid dryrun params: {e}")))?,
        _ => return Err(RpcError::new(INVALID_PARAMS, "missing dryrun params")),
    };
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        let outcome = tokio::task::spawn_blocking(move || {
            dryrun::run(
                &cfg,
                &parsed.path,
                parsed.out.as_deref(),
                parsed.no_chunk,
                None,
            )
        })
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("dryrun join: {e}")))?
        .map_err(|e| write_err("dryrun", e))?;
        serde_json::to_value(&outcome)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("dryrun encode: {e}")))
    })
    .await
}
