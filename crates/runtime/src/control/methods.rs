// SPDX-License-Identifier: Apache-2.0

//! Phase 1 control-plane method table.
//!
//! [`dispatch`] is the only entry point: hand it a parsed
//! [`Request`] and a [`MethodContext`], get back either the JSON
//! payload that becomes the response's `result` or an [`RpcError`].
//!
//! Side effects — `daemon.shutdown` flipping the broadcast, snapshot
//! emission for `events.subscribe` — live in the connection task in
//! [`super::socket`]; this module is otherwise pure over its inputs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bookrack_config::LibrarySelection;
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::reads::info::{LibraryInfoContext, show_library_info};
use bookrack_ops::registry::LibraryRegistry;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::events::EventStreamHandle;
use super::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, Request, RpcError};
use crate::doctor;

/// Workspace version reported to clients through `daemon.version`.
const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Channel names emitted as part of the `events.subscribe` snapshot
/// bundle. Their order is the order clients receive the notifications.
pub const SNAPSHOT_CHANNELS: &[&str] = &[
    "daemon.state",
    "queue.list",
    "library.list",
    "daemon.version",
];

/// Read-mostly handles the dispatcher reaches into. The runtime owns
/// the originals; the dispatcher only clones cheap shared handles.
#[derive(Clone)]
pub struct MethodContext {
    pub registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    pub info_context: LibraryInfoContext,
    pub queue_state: Arc<Mutex<QueueState>>,
    pub event_stream: EventStreamHandle,
    pub shutdown_tx: broadcast::Sender<()>,
    pub started_at_rfc3339: String,
    pub selection: LibrarySelection,
}

/// One of two terminal outcomes a method handler can produce: an
/// inert JSON result, or — for `daemon.shutdown` — a request that the
/// connection writes a final notification before closing.
pub enum DispatchOutcome {
    Result(Value),
    Shutdown(Value),
}

/// Phase 1 method router. Method names are matched verbatim against
/// the table in [`docs/control-plane.md`](../../../../docs/control-plane.md).
pub async fn dispatch(req: &Request, ctx: &MethodContext) -> Result<DispatchOutcome, RpcError> {
    match req.method.as_str() {
        "daemon.version" => Ok(DispatchOutcome::Result(daemon_version(ctx))),
        "daemon.shutdown" => Ok(DispatchOutcome::Shutdown(daemon_shutdown(ctx))),
        "status" => Ok(DispatchOutcome::Result(status(ctx))),
        "doctor.gather" => Ok(DispatchOutcome::Result(doctor_gather(ctx).await)),
        "queue.list" => Ok(DispatchOutcome::Result(queue_list(&req.params, ctx)?)),
        "library.list" => Ok(DispatchOutcome::Result(library_list(ctx)?)),
        "library.info" => Ok(DispatchOutcome::Result(
            library_info(&req.params, ctx).await?,
        )),
        "events.subscribe" => Ok(DispatchOutcome::Result(json!({ "subscribed": true }))),
        "events.snapshot" => Ok(DispatchOutcome::Result(events_snapshot(&req.params, ctx)?)),
        other => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        )),
    }
}

/// Build the snapshot map used both for `events.subscribe`'s burst
/// and for `events.snapshot`'s response.
pub fn snapshot_for(channel: &str, ctx: &MethodContext) -> Option<Value> {
    match channel {
        "daemon.state" => Some(serde_json::to_value(ctx.event_stream.current_state()).ok()?),
        "queue.list" => {
            let state = ctx.queue_state.lock().ok()?;
            Some(serde_json::to_value(&*state).ok()?)
        }
        "library.list" => ctx.registry.list().ok().map(library_list_value),
        "daemon.version" => Some(daemon_version(ctx)),
        _ => None,
    }
}

fn daemon_version(ctx: &MethodContext) -> Value {
    json!({
        "version": DAEMON_VERSION,
        "started_at": ctx.started_at_rfc3339,
    })
}

fn daemon_shutdown(ctx: &MethodContext) -> Value {
    let _ = ctx.shutdown_tx.send(());
    Value::Null
}

fn status(ctx: &MethodContext) -> Value {
    let queue_pending;
    let queue_running;
    {
        let state = ctx.queue_state.lock();
        let (pending, running) = match state {
            Ok(state) => (
                state
                    .jobs
                    .iter()
                    .filter(|j| matches!(j.state, bookrack_core::queue::JobState::Pending))
                    .count(),
                state
                    .jobs
                    .iter()
                    .filter(|j| matches!(j.state, bookrack_core::queue::JobState::Running))
                    .count(),
            ),
            Err(_) => (0, 0),
        };
        queue_pending = pending as u32;
        queue_running = running as u32;
    }
    json!({
        "state": ctx.event_stream.current_state(),
        "queue_pending": queue_pending,
        "queue_running": queue_running,
    })
}

async fn doctor_gather(ctx: &MethodContext) -> Value {
    let report = doctor::gather(&ctx.selection).await;
    serde_json::to_value(report).unwrap_or(Value::Null)
}

#[derive(Debug, Default, Deserialize)]
struct QueueListParams {
    #[serde(default)]
    limit: Option<u32>,
}

fn queue_list(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: QueueListParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid queue.list params: {e}"))
        })?,
        _ => QueueListParams::default(),
    };
    let state = ctx
        .queue_state
        .lock()
        .map_err(|_| RpcError::new(INTERNAL_ERROR, "queue state lock poisoned"))?;
    let mut jobs = state.jobs.clone();
    if let Some(limit) = parsed.limit {
        jobs.truncate(limit as usize);
    }
    Ok(json!({
        "schema_version": state.schema_version,
        "paused": state.paused,
        "jobs": jobs,
    }))
}

fn library_list(ctx: &MethodContext) -> Result<Value, RpcError> {
    let summaries = ctx
        .registry
        .list()
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("registry list failed: {e}")))?;
    Ok(library_list_value(summaries))
}

fn library_list_value(summaries: Vec<bookrack_ops::registry::LibrarySummary>) -> Value {
    let rows: Vec<Value> = summaries
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "default": s.is_default,
                "dimension": s.dimension,
            })
        })
        .collect();
    Value::Array(rows)
}

#[derive(Debug, Default, Deserialize)]
struct LibraryInfoParams {
    #[serde(default)]
    name: Option<String>,
}

async fn library_info(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: LibraryInfoParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid library.info params: {e}"))
        })?,
        _ => LibraryInfoParams::default(),
    };
    let handle = ctx
        .registry
        .get(parsed.name.as_deref())
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("registry: {e}")))?;
    let info = show_library_info(handle.ops(), ctx.info_context.clone())
        .await
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("library info failed: {e}")))?;
    serde_json::to_value(info)
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("serialise library info: {e}")))
}

#[derive(Debug, Deserialize)]
struct EventsSnapshotParams {
    channels: Vec<String>,
}

fn events_snapshot(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: EventsSnapshotParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                format!("invalid events.snapshot params: {e}"),
            )
        })?,
        _ => return Err(RpcError::new(INVALID_PARAMS, "channels: missing")),
    };
    let mut out = serde_json::Map::new();
    for channel in parsed.channels {
        if let Some(value) = snapshot_for(&channel, ctx) {
            out.insert(channel, value);
        }
    }
    Ok(Value::Object(out))
}

/// Workspace path forwarded into the dispatcher's selection. Exposed
/// for tests that want to fabricate a [`MethodContext`].
#[allow(dead_code)]
pub fn selection_data_dir(selection: &LibrarySelection) -> Option<&PathBuf> {
    selection.data_dir.as_ref()
}
