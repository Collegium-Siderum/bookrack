// SPDX-License-Identifier: Apache-2.0

//! `diagnose.run` — collect the diagnose bundle. The runner mirrors
//! `bookrack diagnose` but returns the bundle path + counts as JSON
//! instead of printing.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use super::super::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use super::MethodContext;

#[derive(Debug, Deserialize)]
pub struct DiagnoseParams {
    #[serde(default)]
    pub out: Option<PathBuf>,
    #[serde(default = "default_days")]
    pub days: u32,
    #[serde(default)]
    pub no_scrub: bool,
}

fn default_days() -> u32 {
    bookrack_diagnose::DEFAULT_DAYS
}

pub async fn run(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: DiagnoseParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("diagnose.run params: {e}")))?,
        _ => DiagnoseParams {
            out: None,
            days: default_days(),
            no_scrub: false,
        },
    };
    let cfg = ctx.cfg.clone();
    let opts = bookrack_diagnose::Options {
        days: parsed.days,
        scrub: !parsed.no_scrub,
        out: parsed.out,
        now: None,
    };
    let report = tokio::task::spawn_blocking(move || bookrack_diagnose::collect(&cfg, &opts))
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("diagnose join: {e}")))?
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("diagnose collect: {e:#}")))?;
    Ok(json!({
        "out_path": report.out_path,
        "files": report.files,
        "scrubbed": report.scrubbed,
    }))
}
