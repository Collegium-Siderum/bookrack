// SPDX-License-Identifier: Apache-2.0

//! `queue.pause` / `queue.resume` / `queue.clear` JSON-RPC handlers.
//!
//! Each method mutates either the worker-loop pause flag, the on-disk
//! queue document, or both, and emits a single
//! [`crate::control::events::Event::QueueTick`] so connected clients
//! observe the new state without polling. The handlers reuse the same
//! [`crate::queue::cancel_all_pending`] primitive that the in-process
//! REPL `queue clear` once called directly, keeping the trim semantics
//! in one place: only `Pending` rows are turned into `Cancelled`; rows
//! already `Running`, `Done`, `Failed`, or `Cancelled` are left alone.

use std::sync::atomic::Ordering;

use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use ts_rs::TS;

use super::MethodContext;
use crate::control::events::Event;
use crate::control::jsonrpc::{INTERNAL_ERROR, RpcError};
use crate::queue::{cancel_all_pending, derive_tick, save_atomic};

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PauseResponse {
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct ClearResponse {
    pub paused: bool,
    pub cleared: usize,
}

pub async fn pause(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    set_paused(ctx, true)
}

pub async fn resume(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    set_paused(ctx, false)
}

pub async fn clear(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let (cleared, tick) = {
        let mut guard = ctx.queue_state.lock().expect("queue state mutex poisoned");
        let cleared = cancel_all_pending(&mut guard);
        save_atomic(&guard, &ctx.queue_state_path)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("persist queue state: {e:#}")))?;
        let tick = derive_tick(&guard, None);
        (cleared, tick)
    };
    ctx.event_stream.publish(Event::QueueTick(tick));
    let paused = ctx.queue_paused.load(Ordering::Acquire);
    serde_json::to_value(ClearResponse { paused, cleared }).map_err(|e| {
        RpcError::new(
            INTERNAL_ERROR,
            format!("serialise queue.clear response: {e}"),
        )
    })
}

fn set_paused(ctx: &MethodContext, paused: bool) -> Result<Value, RpcError> {
    ctx.queue_paused.store(paused, Ordering::Release);
    let tick = {
        let mut guard = ctx.queue_state.lock().expect("queue state mutex poisoned");
        guard.paused = paused;
        save_atomic(&guard, &ctx.queue_state_path)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("persist queue state: {e:#}")))?;
        derive_tick(&guard, None)
    };
    ctx.event_stream.publish(Event::QueueTick(tick));
    serde_json::to_value(PauseResponse { paused }).map_err(|e| {
        RpcError::new(
            INTERNAL_ERROR,
            format!("serialise queue.pause response: {e}"),
        )
    })
}
