// SPDX-License-Identifier: Apache-2.0

//! `library.fork` — clone the active library into a sibling data
//! root and register it in the user's library registry. The MCP
//! parity work in Phase 5 reuses this same handler.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use super::super::jsonrpc::{INVALID_PARAMS, RpcError};
use super::MethodContext;
use super::run_write;
use crate::cmd::libraries::CopyMode;

#[derive(Debug, Deserialize)]
pub struct ForkParams {
    pub new_name: String,
    pub data_dir: PathBuf,
    /// `"hardlink"` (default) or `"copy"`. Mirrors the cli's
    /// `--copy-mode` flag.
    #[serde(default = "default_copy_mode")]
    pub copy_mode: String,
    /// Must be `true`; the control-plane runner does not prompt for
    /// confirmation. The cli client holds any interactive prompt and
    /// forwards `yes: true` once the operator confirms.
    #[serde(default)]
    pub yes: bool,
}

fn default_copy_mode() -> String {
    "hardlink".to_string()
}

pub async fn fork(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let raw = params
        .clone()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "library.fork: missing params"))?;
    let parsed: ForkParams = serde_json::from_value(raw)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("library.fork params: {e}")))?;
    if !parsed.yes {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "library.fork requires yes=true; the client is responsible for any operator prompt",
        ));
    }
    let mode = match parsed.copy_mode.as_str() {
        "hardlink" => CopyMode::Hardlink,
        "copy" => CopyMode::Copy,
        other => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!("library.fork copy_mode: expected hardlink or copy, got {other:?}"),
            ));
        }
    };
    let cfg = ctx.cfg.clone();
    let target = parsed.data_dir.clone();
    let new_name = parsed.new_name.clone();
    run_write(ctx, move || async move {
        crate::cmd::libraries::fork(&cfg, &new_name, &target, mode, true, |_| Ok(true)).map_err(
            |e| {
                RpcError::new(
                    crate::control::jsonrpc::INTERNAL_ERROR,
                    format!("library.fork: {e:#}"),
                )
            },
        )?;
        Ok(json!({
            "new_name": new_name,
            "data_dir": target,
        }))
    })
    .await
}
