// SPDX-License-Identifier: Apache-2.0

//! `library.fork` — clone the active library into a sibling data
//! root and register it in the user's library registry. The MCP
//! parity work in Phase 5 reuses this same handler.
//!
//! `library.set_default` — re-point the registry's default-library
//! pointer at one of its known libraries. The change is persisted to
//! the on-disk registry, then the daemon's in-memory pointer — a cache
//! of that on-disk value — is refreshed, so the default survives a
//! daemon restart and the running daemon's routing follows immediately.

use std::path::PathBuf;

use bookrack_config::{registry_target_path, set_default_library};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::super::error_map::{config_err, registry_err};
use super::super::events::Event;
use super::super::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
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

/// Re-point the default-library pointer at `name`, persisting it to the
/// registry.
///
/// The registry file is the single home of the default. The name is
/// validated against the daemon's registered libraries first, so an
/// unknown name is rejected before any write; the change is then written
/// to the on-disk registry, and only afterwards is the daemon's
/// in-memory pointer — a cache of the on-disk value — refreshed. Writing
/// disk before memory keeps the truth ahead of its cache: a memory flip
/// that outran a failed disk write would silently evaporate on restart.
/// Fires a `library.changed` event so subscribers refresh their view.
pub async fn set_default(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let raw = params
        .clone()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "library.set_default: missing params"))?;
    let parsed: LibrarySetDefaultParams = serde_json::from_value(raw)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("library.set_default params: {e}")))?;

    // Validate against the registered libraries before touching disk, so
    // an unknown name fails without a write.
    ctx.registry.get(Some(&parsed.name)).map_err(registry_err)?;

    // Persist to the registry, then refresh the in-memory cache.
    let registry_path = registry_target_path().ok_or_else(|| {
        RpcError::new(
            INTERNAL_ERROR,
            "library.set_default: no registry file to persist the default",
        )
    })?;
    set_default_library(&registry_path, &parsed.name).map_err(config_err)?;
    ctx.registry
        .set_default(&parsed.name)
        .map_err(registry_err)?;

    ctx.event_stream.publish(Event::LibraryChanged {
        library: parsed.name.clone(),
    });
    Ok(json!({ "ok": true, "name": parsed.name }))
}
