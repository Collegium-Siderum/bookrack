// SPDX-License-Identifier: Apache-2.0

//! `papers.dryrun` JSON-RPC handler.
//!
//! Peer of [`super::dryrun::run`] for the paper pipeline.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::papers_dryrun;
use crate::control::error_map::{registry_err, write_err};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PapersDryrunParams {
    #[cfg_attr(test, ts(type = "string"))]
    path: PathBuf,
    #[serde(default)]
    #[cfg_attr(test, ts(type = "string | null"))]
    out: Option<PathBuf>,
    #[serde(default)]
    no_chunk: bool,
    /// The library this call acts on. Absent means the registry's
    /// current default — the library the daemon was brought up under,
    /// unless `library.set_default` has moved it since.
    #[serde(default)]
    #[cfg_attr(test, ts(type = "string | null"))]
    library: Option<String>,
}

pub async fn run(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersDryrunParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid papers.dryrun params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "missing papers.dryrun params",
            ));
        }
    };
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    run_write(ctx, handle.name(), move || async move {
        let outcome = tokio::task::spawn_blocking(move || {
            papers_dryrun::run(&cfg, &parsed.path, parsed.out.as_deref(), parsed.no_chunk)
        })
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("papers.dryrun join: {e}")))?
        .map_err(|e| write_err("papers.dryrun", e))?;
        serde_json::to_value(&outcome)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("papers.dryrun encode: {e}")))
    })
    .await
}
