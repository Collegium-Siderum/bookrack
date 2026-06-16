// SPDX-License-Identifier: Apache-2.0

//! `stamps.reconcile` JSON-RPC handler.

use serde_json::{Value, json};

use super::{MethodContext, run_write};
use crate::cmd::stamps;
use crate::control::error_map::write_err;
use crate::control::jsonrpc::RpcError;

pub async fn reconcile(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        stamps::reconcile(&cfg)
            .await
            .map_err(|e| write_err("stamps.reconcile", e))?;
        Ok(json!({ "ok": true }))
    })
    .await
}
