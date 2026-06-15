// SPDX-License-Identifier: Apache-2.0

//! Paper-side metadata curation methods on the control plane. This
//! file currently exposes `papers.metadata.reaudit`; the eight write
//! actions (`set`, `clear`, `void`, `ack`, `approve`, `reject`,
//! `contributor_add`, `contributor_remove`) land in a follow-up.

use serde::Deserialize;
use serde_json::{Value, json};

use super::MethodContext;
use crate::audit_helpers::{load_paper_audit_data, load_paper_audit_profile};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

#[derive(Debug, Deserialize)]
pub struct PapersMetadataReauditParams {
    /// The paper intake to re-audit.
    intake_id: i64,
    /// Optional named profile (`default` / `trust-source` / `strict`).
    /// When absent the daemon's effective profile (default + overlay)
    /// is used.
    #[serde(default)]
    audit_profile: Option<String>,
    /// Optional library name; defaults to the daemon's current
    /// library.
    #[serde(default)]
    library: Option<String>,
}

pub async fn reaudit(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: PapersMetadataReauditParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                format!("invalid papers.metadata.reaudit params: {e}"),
            )
        })?,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "missing papers.metadata.reaudit params",
            ));
        }
    };

    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("library handle: {e}")))?;

    let profile = load_paper_audit_profile(&ctx.cfg, parsed.audit_profile.as_deref());
    let data = load_paper_audit_data(&ctx.cfg);

    let outcome = handle
        .reaudit_paper(parsed.intake_id, &profile, &data)
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("reaudit_paper: {e:#}")))?;

    Ok(json!({
        "intake_id": outcome.intake_id,
        "verdict": outcome.verdict,
        "previous_verdict": outcome.previous_verdict,
        "confidence": outcome.confidence,
        "previous_confidence": outcome.previous_confidence,
    }))
}
