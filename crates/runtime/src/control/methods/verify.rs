// SPDX-License-Identifier: Apache-2.0

//! `verify.run` — JSON serialisation of the verify report that
//! `bookrack verify` (Phase 0 binary) used to render directly.

use serde::Deserialize;
use serde_json::Value;
#[cfg(test)]
use ts_rs::TS;

use super::super::jsonrpc::RpcError;
use super::MethodContext;
use crate::control::error_map::registry_err;

/// Build the cross-store verify report and return it as JSON.
///
/// Verify opens each store through its own read-only door, which takes
/// no write lock and shares nothing with the handles a write command
/// mutates, so it runs alongside one rather than queueing behind it.
/// The report walks every intake row and stats its file, so it is built
/// on a blocking executor and the dispatcher's reactor stays free.
pub async fn run(library: Option<&str>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let handle = ctx.registry.get(library).map_err(registry_err)?;
    let cfg = handle.cfg_arc();
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

/// Which library to verify.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct VerifyParams {
    /// The library this call acts on. Absent means the registry's
    /// current default — the library the daemon was brought up under,
    /// unless `library.set_default` has moved it since.
    #[serde(default)]
    #[cfg_attr(test, ts(type = "string | null"))]
    library: Option<String>,
}

/// Adapter to the uniform dispatch signature.
pub async fn run_rpc(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: VerifyParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(
                crate::control::jsonrpc::INVALID_PARAMS,
                format!("invalid verify params: {e}"),
            )
        })?,
        _ => VerifyParams::default(),
    };
    run(parsed.library.as_deref(), ctx).await
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

        let value = run(None, &ctx)
            .await
            .expect("verify answers during a write");
        assert_eq!(
            value.get("not_initialised").and_then(Value::as_bool),
            Some(true),
            "{value}"
        );
        drop(held);
    }
}
