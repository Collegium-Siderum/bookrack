// SPDX-License-Identifier: Apache-2.0

//! `papers.stamps_reconcile` JSON-RPC handler.
//!
//! Peer of [`super::stamps::reconcile`] for the paper pipeline.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::papers_stamps;
use crate::control::error_map::registry_err;
use crate::control::error_map::write_err;
use crate::control::jsonrpc::{INVALID_PARAMS, RpcError};

/// Which library's stamps to reconcile.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct ReconcileParams {
    /// The library this call acts on. Absent means the registry's
    /// current default — the library the daemon was brought up under,
    /// unless `library.set_default` has moved it since.
    #[serde(default)]
    #[cfg_attr(test, ts(type = "string | null"))]
    library: Option<String>,
}

pub async fn reconcile(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: ReconcileParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                format!("invalid papers.stamps.reconcile params: {e}"),
            )
        })?,
        _ => ReconcileParams::default(),
    };
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    run_write(ctx, handle.name(), move || async move {
        papers_stamps::reconcile(&cfg)
            .await
            .map_err(|e| write_err("papers.stamps_reconcile", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}
