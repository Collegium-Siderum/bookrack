// SPDX-License-Identifier: Apache-2.0

//! `verify.run` — JSON serialisation of the verify report that
//! `bookrack verify` (Phase 0 binary) used to render directly.

use serde_json::Value;

use super::super::jsonrpc::RpcError;
use super::MethodContext;

/// Build the cross-store verify report and return it as JSON.
///
/// Verify opens each store through its own read-only door, which takes
/// no write lock and shares nothing with the handles a write command
/// mutates, so it runs alongside one rather than queueing behind it.
/// The report walks every intake row and stats its file, so it is built
/// on a blocking executor and the dispatcher's reactor stays free.
pub async fn run(ctx: &MethodContext) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    let report = tokio::task::spawn_blocking(move || crate::cmd::verify::build_verify_report(&cfg))
        .await
        .map_err(|e| {
            RpcError::new(
                crate::control::jsonrpc::INTERNAL_ERROR,
                format!("verify report join failed: {e}"),
            )
        })?;
    serde_json::to_value(report).map_err(|e| {
        RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            format!("encode verify report: {e}"),
        )
    })
}

/// Adapter to the uniform dispatch signature.
pub async fn run_rpc(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    run(ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::methods::test_method_context;

    #[tokio::test]
    async fn verify_answers_while_a_write_holds_the_guard() {
        // Verify opens its own read-only handles, so it contends with
        // nothing a write command holds. Funnelling it through the write
        // mutex would make a health check on a library with a long
        // ingest in flight answer `busy` instead of answering.
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_method_context(dir.path(), None);
        let held = ctx.write_guard.clone().lock_owned().await;

        let value = run(&ctx).await.expect("verify answers during a write");
        assert_eq!(
            value.get("not_initialised").and_then(Value::as_bool),
            Some(true),
            "{value}"
        );
        drop(held);
    }
}
