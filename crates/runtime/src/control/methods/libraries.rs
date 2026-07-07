// SPDX-License-Identifier: Apache-2.0

//! `library.fork` — clone the active library into a sibling data
//! root and register it in the user's library registry. The MCP
//! parity work in Phase 5 reuses this same handler.
//!
//! `library.set_default` — re-point the registry's default-library
//! pointer at one of its known libraries. The change lives in the
//! daemon's in-memory registry only; the persistent library
//! registry on disk is untouched, so restarting the daemon picks
//! up whatever the configured `--library` / TOML default says.

use std::path::PathBuf;

use bookrack_ops::registry::RegistryError;
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::super::events::Event;
use super::super::jsonrpc::{INVALID_PARAMS, RpcError};
use super::MethodContext;
use super::run_write;
use crate::cmd::libraries::CopyMode;

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct LibraryForkParams {
    pub new_name: String,
    #[cfg_attr(test, ts(type = "string"))]
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
    let parsed: LibraryForkParams = serde_json::from_value(raw)
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

#[derive(Debug, Deserialize)]
pub struct LibrarySetDefaultParams {
    pub name: String,
}

/// Re-point the daemon's in-memory default-library pointer at `name`.
/// This affects only the running daemon session and is not persisted;
/// the on-disk registry default is written by the CLI's offline
/// `libraries default`, which owns registry persistence. The daemon's
/// primary `library_name` (used as a fallback when an RPC caller omits
/// `library`) is unchanged — the call is advisory and fires a
/// `library.changed` event so subscribers can refresh their view of
/// which library the daemon now reports as default.
pub async fn set_default(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let raw = params
        .clone()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "library.set_default: missing params"))?;
    let parsed: LibrarySetDefaultParams = serde_json::from_value(raw)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("library.set_default params: {e}")))?;
    match ctx.registry.set_default(&parsed.name) {
        Ok(()) => {
            ctx.event_stream.publish(Event::LibraryChanged {
                library: parsed.name.clone(),
            });
            Ok(json!({ "ok": true, "name": parsed.name }))
        }
        Err(err @ RegistryError::LibraryUnknown { .. }) => {
            Err(RpcError::new(INVALID_PARAMS, err.to_string()))
        }
        Err(err) => Err(RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            err.to_string(),
        )),
    }
}
