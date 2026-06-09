// SPDX-License-Identifier: Apache-2.0

//! `stamps.reconcile` JSON-RPC handler.

use serde_json::{Value, json};

use super::{MethodContext, run_write};
use crate::cmd::stamps;
use crate::control::jsonrpc::{INTERNAL_ERROR, RpcError};

pub async fn reconcile(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        stamps::reconcile(&cfg).await.map_err(|e| {
            RpcError::new(INTERNAL_ERROR, format!("stamps.reconcile failed: {e:#}"))
        })?;
        Ok(json!({ "ok": true }))
    })
    .await
}
