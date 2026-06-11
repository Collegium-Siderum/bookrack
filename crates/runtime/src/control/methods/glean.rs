// SPDX-License-Identifier: Apache-2.0

//! `glean.submit` JSON-RPC handler.
//!
//! Peer of [`super::ingest::submit`] for the paper pipeline. Enqueues
//! one `QueueJob` per path with `kind = Paper` so the worker loop
//! dispatches it through [`bookrack_ops::registry::LibraryHandle::glean_paper`]
//! instead of `ingest_book`. Lifecycle methods (`ingest.cancel`,
//! `queue.list`, `queue.pause`, `queue.resume`, `queue.clear`)
//! already cover both kinds, so there is no `glean.cancel` peer.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::MethodContext;
use super::ingest::PriorityRepr;
use crate::control::events::Event;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::queue;

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct GleanSubmitParams {
    #[cfg_attr(test, ts(type = "Array<string>"))]
    paths: Vec<PathBuf>,
    #[serde(default)]
    library: Option<String>,
    #[serde(default)]
    priority: Option<PriorityRepr>,
    #[serde(default)]
    force: bool,
}

pub async fn submit(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: GleanSubmitParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid glean.submit params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(INVALID_PARAMS, "missing glean.submit params"));
        }
    };
    if parsed.paths.is_empty() {
        return Err(RpcError::new(INVALID_PARAMS, "paths: must be non-empty"));
    }
    let library = parsed.library.unwrap_or_else(|| ctx.library_name.clone());
    let priority = parsed
        .priority
        .map(PriorityRepr::into_priority)
        .unwrap_or_default();
    let ids = {
        let mut guard = ctx
            .queue_state
            .lock()
            .map_err(|_| RpcError::new(INTERNAL_ERROR, "queue state lock poisoned"))?;
        let ids = queue::enqueue_files(
            &mut guard,
            &parsed.paths,
            &library,
            bookrack_core::ItemKind::Paper,
            priority,
            parsed.force,
        );
        queue::save_atomic(&guard, &ctx.queue_state_path)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("persist queue state: {e}")))?;
        let tick = super::ingest::derive_tick(&guard, None);
        ctx.event_stream.publish(Event::QueueTick(tick));
        ids
    };
    Ok(json!({ "job_ids": ids }))
}
