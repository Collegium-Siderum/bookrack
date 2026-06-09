// SPDX-License-Identifier: Apache-2.0

//! `metadata.{set,clear,ack,approve,reject}` JSON-RPC handlers.
//!
//! Each method maps onto the matching variant of
//! [`crate::cmd::metadata::WriteMetadataAction`] and runs through
//! [`super::run_write`] so the write mutex, daemon-state flag, and
//! broadcast notifications are managed in one place.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{MethodContext, run_write};
use crate::cmd::metadata::{WriteMetadataAction, run_write as run_metadata};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

#[derive(Debug, Deserialize)]
struct SetParams {
    book: i64,
    field: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct ClearParams {
    book: i64,
    field: String,
}

#[derive(Debug, Deserialize)]
struct AckParams {
    book: i64,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ApproveParams {
    book: i64,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RejectParams {
    book: i64,
    reason: String,
}

pub async fn set(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: SetParams = parse(params, "metadata.set")?;
    let action = WriteMetadataAction::Set {
        book: parsed.book,
        field: parsed.field,
        value: parsed.value,
    };
    run_metadata_action(ctx, action).await
}

pub async fn clear(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: ClearParams = parse(params, "metadata.clear")?;
    let action = WriteMetadataAction::Clear {
        book: parsed.book,
        field: parsed.field,
    };
    run_metadata_action(ctx, action).await
}

pub async fn ack(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: AckParams = parse(params, "metadata.ack")?;
    let action = WriteMetadataAction::Ack {
        book: parsed.book,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn approve(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: ApproveParams = parse(params, "metadata.approve")?;
    let action = WriteMetadataAction::Approve {
        book: parsed.book,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn reject(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: RejectParams = parse(params, "metadata.reject")?;
    let action = WriteMetadataAction::Reject {
        book: parsed.book,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

fn parse<T: serde::de::DeserializeOwned>(
    params: &Option<Value>,
    method: &str,
) -> Result<T, RpcError> {
    match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid {method} params: {e}"))),
        _ => Err(RpcError::new(
            INVALID_PARAMS,
            format!("missing {method} params"),
        )),
    }
}

async fn run_metadata_action(
    ctx: &MethodContext,
    action: WriteMetadataAction,
) -> Result<Value, RpcError> {
    let cfg = ctx.cfg.clone();
    run_write(ctx, || async move {
        run_metadata(&cfg, action, None)
            .await
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("metadata write failed: {e:#}")))?;
        Ok(json!({ "ok": true }))
    })
    .await
}
