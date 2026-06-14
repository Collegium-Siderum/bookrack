// SPDX-License-Identifier: Apache-2.0

//! `papers.stamps_reconcile` JSON-RPC handler.
//!
//! Peer of [`super::stamps::reconcile`] for the paper pipeline.

use serde_json::{Value, json};

use super::{MethodContext, run_write};
use crate::cmd::papers_stamps;
use crate::control::jsonrpc::{INTERNAL_ERROR, RpcError};

pub async fn reconcile(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        papers_stamps::reconcile(&cfg).await.map_err(|e| {
            RpcError::new(
                INTERNAL_ERROR,
                format!("papers.stamps_reconcile failed: {e:#}"),
            )
        })?;
        Ok(json!({ "ok": true }))
    })
    .await
}
