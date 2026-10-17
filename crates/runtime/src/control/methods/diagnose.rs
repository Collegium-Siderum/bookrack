// SPDX-License-Identifier: Apache-2.0

//! `diagnose.run` — collect the diagnose bundle for one library. The
//! runner mirrors `bookrack diagnose` but returns the bundle path +
//! counts as JSON instead of printing.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use super::super::error_map::registry_err;
use super::super::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use super::MethodContext;

#[derive(Debug, Deserialize)]
pub struct DiagnoseRunParams {
    /// The library whose catalog, stores, and audit overlay the bundle
    /// snapshots, and under whose root it lands unless `out` says
    /// otherwise. Absent means the registry's current default.
    #[serde(default)]
    pub library: Option<String>,
    #[serde(default)]
    pub out: Option<PathBuf>,
    #[serde(default = "default_days")]
    pub days: u32,
    #[serde(default)]
    pub no_scrub: bool,
}

routed_params!(DiagnoseRunParams);

fn default_days() -> u32 {
    bookrack_diagnose::DEFAULT_DAYS
}

pub async fn run(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: DiagnoseRunParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("diagnose.run params: {e}")))?,
        _ => DiagnoseRunParams {
            library: None,
            out: None,
            days: default_days(),
            no_scrub: false,
        },
    };
    // The library-level half of the bundle — catalog snapshot, store
    // stamps, audit overlay, landing place — comes from the selected
    // handle's own configuration. The process-level half — logs and
    // crash reports — is read from the daemon state directory whatever
    // library was named, and may mention every library the daemon
    // serves, so their roots are handed to the scrubber.
    let handle = super::select_library(ctx, parsed.library.as_deref())?;
    let cfg = handle.cfg_arc();
    let library = handle.name().to_string();
    let other_roots = ctx
        .registry
        .list()
        .map_err(registry_err)?
        .into_iter()
        .filter(|summary| summary.name != library)
        .map(|summary| summary.data_dir)
        .collect();
    let scope = bookrack_diagnose::Scope {
        library: Some(library.clone()),
        other_roots,
    };
    let opts = bookrack_diagnose::Options {
        days: parsed.days,
        scrub: !parsed.no_scrub,
        out: parsed.out,
        now: None,
    };
    let report =
        tokio::task::spawn_blocking(move || bookrack_diagnose::collect(&cfg, &opts, &scope))
            .await
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("diagnose join: {e}")))?
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("diagnose collect: {e:#}")))?;
    Ok(json!({
        "library": library,
        "out_path": report.out_path,
        "files": report.files,
        "scrubbed": report.scrubbed,
        "scrub_gaps": report.scrub_gaps,
    }))
}
