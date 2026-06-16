// SPDX-License-Identifier: Apache-2.0

//! `verify.run` — JSON serialisation of the verify report that
//! `bookrack verify` (Phase 0 binary) used to render directly.

use serde_json::Value;

use super::super::jsonrpc::RpcError;
use super::MethodContext;
use super::run_write;

/// Build the cross-store verify report and return it as JSON.
///
/// Verify only reads, but the catalog handle it opens is the same
/// shared-state catalog the write commands mutate. Funnel it through
/// [`run_write`] so it cannot overlap with a write that is in flight.
pub async fn run(ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    run_write(ctx, move || async move {
        let report = crate::cmd::verify::build_verify_report(&cfg);
        serde_json::to_value(report).map_err(|e| {
            RpcError::new(
                crate::control::jsonrpc::INTERNAL_ERROR,
                format!("encode verify report: {e}"),
            )
        })
    })
    .await
}

/// Adapter to the uniform dispatch signature.
pub async fn run_rpc(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    run(ctx).await
}
