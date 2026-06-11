// SPDX-License-Identifier: Apache-2.0

//! `metadata.{set,clear,ack,approve,reject}` JSON-RPC handlers.
//!
//! Each method maps onto the matching variant of
//! [`crate::cmd::metadata::WriteMetadataAction`] and runs through
//! [`super::run_write`] so the write mutex, daemon-state flag, and
//! broadcast notifications are managed in one place.

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::{MethodContext, run_write};
use crate::cmd::metadata::{WriteMetadataAction, run_write as run_metadata};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataSetParams {
    book: i64,
    field: String,
    value: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataClearParams {
    book: i64,
    field: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataVoidParams {
    book: i64,
    field: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataReauditParams {
    book: i64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataContributorAddParams {
    book: i64,
    role: String,
    name: String,
    #[serde(default)]
    nationality: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataContributorRemoveParams {
    book: i64,
    contributor_id: i64,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataAckParams {
    book: i64,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataApproveParams {
    book: i64,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct MetadataRejectParams {
    book: i64,
    reason: String,
}

pub async fn set(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: MetadataSetParams = parse(params, "metadata.set")?;
    let action = WriteMetadataAction::Set {
        book: parsed.book,
        field: parsed.field,
        value: parsed.value,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn clear(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: MetadataClearParams = parse(params, "metadata.clear")?;
    let action = WriteMetadataAction::Clear {
        book: parsed.book,
        field: parsed.field,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn void(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: MetadataVoidParams = parse(params, "metadata.void")?;
    let action = WriteMetadataAction::Void {
        book: parsed.book,
        field: parsed.field,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn reaudit(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: MetadataReauditParams = parse(params, "metadata.reaudit")?;
    let action = WriteMetadataAction::Reaudit { book: parsed.book };
    run_metadata_action(ctx, action).await
}

pub async fn contributor_add(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let parsed: MetadataContributorAddParams = parse(params, "metadata.contributor_add")?;
    let action = WriteMetadataAction::ContributorAdd {
        book: parsed.book,
        role: parsed.role,
        name: parsed.name,
        nationality: parsed.nationality,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn contributor_remove(
    params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    let parsed: MetadataContributorRemoveParams = parse(params, "metadata.contributor_remove")?;
    let action = WriteMetadataAction::ContributorRemove {
        book: parsed.book,
        contributor_id: parsed.contributor_id,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn ack(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: MetadataAckParams = parse(params, "metadata.ack")?;
    let action = WriteMetadataAction::Ack {
        book: parsed.book,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn approve(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: MetadataApproveParams = parse(params, "metadata.approve")?;
    let action = WriteMetadataAction::Approve {
        book: parsed.book,
        reason: parsed.reason,
    };
    run_metadata_action(ctx, action).await
}

pub async fn reject(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: MetadataRejectParams = parse(params, "metadata.reject")?;
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
