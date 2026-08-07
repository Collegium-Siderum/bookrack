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

use super::super::error_map::{config_err, mount_err, registry_err, write_err};
use super::super::events::Event;
use super::super::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, NOT_READY, RpcError};
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
    /// The library being forked — the source of the clone. Absent
    /// means the registry's current default. `library.fork` is the one
    /// method that legitimately holds two libraries at once, so the
    /// source is named explicitly rather than inherited from whichever
    /// library the daemon came up under.
    #[serde(default)]
    #[cfg_attr(test, ts(type = "string | null"))]
    library: Option<String>,
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
    let registry_path = registry_target_path().ok_or_else(|| {
        RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            "library.fork: no registry location: set BOOKRACK_REGISTRY=<path> or ensure the \
             platform config directory is available",
        )
    })?;
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    let target = parsed.data_dir.clone();
    let new_name = parsed.new_name.clone();
    run_write(ctx, handle.name(), move || async move {
        crate::cmd::libraries::fork(&cfg, &new_name, &target, &registry_path, mode, true, |_| {
            Ok(true)
        })
        .map_err(|e| write_err("library.fork", e))?;
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

#[derive(Debug, Deserialize)]
pub struct LibraryMountParams {
    pub name: String,
}

/// Open a registered library and add it to the set this daemon serves.
///
/// The name is a registry name, never a path: the daemon resolves it
/// through the registry, so what it mounts is what the registry
/// declares, and registering a new root stays a separate act with its
/// own failure modes.
///
/// Runs through [`run_write`] like every other change to what the
/// daemon serves: the write mutex keeps a mount from racing another
/// write, MCP is paused for its duration, and the `library.changed`
/// event a subscriber needs to refresh its view of the library set is
/// published on success by the wrapper itself.
pub async fn mount(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let name = mount_target("library.mount", params)?;
    let mounter = require_mounter("library.mount", ctx)?;
    let target = name.clone();
    run_write(ctx, &name, move || async move {
        mounter.mount(&target).await.map_err(mount_err)?;
        tracing::info!(library = %target, "library mounted at runtime");
        Ok(json!({ "ok": true, "name": target }))
    })
    .await
}

/// Stop serving a library and let go of its data root.
///
/// Refuses the registry default, the library the daemon came up under,
/// and a library with queued work; see [`crate::mount::Mounter::unmount`]
/// for why each of the three would otherwise leave the daemon
/// describing something it no longer serves.
///
/// The root lock is released when the last caller holding the library's
/// handle is done with it, which may be after this call returns: an
/// in-flight read keeps the root held until it finishes.
pub async fn unmount(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let name = mount_target("library.unmount", params)?;
    let mounter = require_mounter("library.unmount", ctx)?;
    let primary = ctx.info_context.library_name.clone();
    let queued_jobs = queued_against(ctx, &name)?;
    let target = name.clone();
    run_write(ctx, &name, move || async move {
        let facts = crate::mount::UnmountFacts {
            primary: primary.as_deref(),
            queued_jobs,
        };
        let handle = mounter.unmount(&target, facts).map_err(mount_err)?;
        drop(handle);
        tracing::info!(library = %target, "library unmounted at runtime");
        Ok(json!({ "ok": true, "name": target }))
    })
    .await
}

/// Count the jobs against `library` the worker has not finished with.
fn queued_against(ctx: &MethodContext, library: &str) -> Result<usize, RpcError> {
    let state = ctx.queue_state.lock().map_err(|_| {
        RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            "library.unmount: the queue state lock is poisoned",
        )
    })?;
    Ok(state
        .jobs
        .iter()
        .filter(|job| {
            job.library == library
                && matches!(
                    job.state,
                    bookrack_core::queue::JobState::Pending
                        | bookrack_core::queue::JobState::Running
                )
        })
        .count())
}

fn mount_target(method: &str, params: &Option<Value>) -> Result<String, RpcError> {
    let raw = params
        .clone()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("{method}: missing params")))?;
    let parsed: LibraryMountParams = serde_json::from_value(raw)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("{method} params: {e}")))?;
    Ok(parsed.name)
}

/// The mount capability, or the refusal an entry point without one
/// owes its caller. Same shape as a queue-bound method reaching a
/// daemon that spawned no worker: the method exists, this process
/// cannot serve it.
fn require_mounter(
    method: &str,
    ctx: &MethodContext,
) -> Result<std::sync::Arc<crate::mount::Mounter>, RpcError> {
    ctx.mounter.clone().ok_or_else(|| {
        RpcError::new(
            NOT_READY,
            format!("{method} is not available in this entry point: it dispatches without a daemon bring-up, so it holds no library mounts"),
        )
    })
}

routed_params!(LibraryForkParams);
