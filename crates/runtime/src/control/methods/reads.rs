// SPDX-License-Identifier: Apache-2.0

//! Read-only control-plane methods.
//!
//! Carries the Phase 1 surface — `daemon.version` / `daemon.shutdown`
//! / `status` / `doctor.gather` / `queue.list` / `library.list` /
//! `library.info` / `events.snapshot` — plus the snapshot bundle the
//! socket layer emits on `events.subscribe`. Phase 2 expands the
//! snapshot channels with `queue.tick`, `library.changed`, and
//! `mcp.availability` so a reconnecting client sees the same view as a
//! freshly connected one.

use bookrack_ops::reads::info::show_library_info;
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::MethodContext;
use crate::control::events::{Event, JobOutcomeSummary, QueueTick};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::doctor;

/// Workspace version reported to clients through `daemon.version`.
const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Channel names emitted as part of the `events.subscribe` snapshot
/// bundle. Their order is the order clients receive the notifications.
pub const SNAPSHOT_CHANNELS: &[&str] = &[
    "daemon.state",
    "queue.list",
    "queue.tick",
    "library.list",
    "library.changed",
    "mcp.availability",
    "daemon.version",
];

/// Build the snapshot map used both for `events.subscribe`'s burst
/// and for `events.snapshot`'s response.
pub fn snapshot_for(channel: &str, ctx: &MethodContext) -> Option<Value> {
    match channel {
        "daemon.state" => Some(serde_json::to_value(ctx.event_stream.current_state()).ok()?),
        "queue.list" => {
            let state = ctx.queue_state.lock().ok()?;
            Some(serde_json::to_value(&*state).ok()?)
        }
        "queue.tick" => {
            let state = ctx.queue_state.lock().ok()?;
            let tick = derive_tick_snapshot(&state);
            Some(serde_json::to_value(tick).ok()?)
        }
        "library.list" => ctx.registry.list().ok().map(library_list_value),
        "library.changed" => Some(json!({ "library": ctx.library_name })),
        "mcp.availability" => Some(Event::McpAvailability { paused: false }.value()),
        "daemon.version" => Some(daemon_version(ctx)),
        _ => None,
    }
}

pub fn daemon_version(ctx: &MethodContext) -> Value {
    json!({
        "version": DAEMON_VERSION,
        "started_at": ctx.started_at_rfc3339,
    })
}

pub fn daemon_shutdown(ctx: &MethodContext) -> Value {
    let _ = ctx.shutdown_tx.send(());
    Value::Null
}

pub fn status(ctx: &MethodContext) -> Value {
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

pub async fn doctor_gather(ctx: &MethodContext) -> Value {
    let report = doctor::gather(&ctx.selection).await;
    serde_json::to_value(report).unwrap_or(Value::Null)
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct QueueListParams {
    #[serde(default)]
    limit: Option<u32>,
}

pub fn queue_list(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
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

pub fn library_list(ctx: &MethodContext) -> Result<Value, RpcError> {
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
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct LibraryInfoParams {
    #[serde(default)]
    name: Option<String>,
}

pub async fn library_info(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
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
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct EventsSnapshotParams {
    channels: Vec<String>,
}

pub fn events_snapshot(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
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

fn derive_tick_snapshot(state: &bookrack_core::queue::QueueState) -> QueueTick {
    let mut pending = 0u32;
    let mut running = 0u32;
    let mut current = None;
    let mut last_finished: Option<JobOutcomeSummary> = None;
    for job in &state.jobs {
        match job.state {
            bookrack_core::queue::JobState::Pending => pending += 1,
            bookrack_core::queue::JobState::Running => {
                running += 1;
                if current.is_none() {
                    current = Some(job.id.clone());
                }
            }
            bookrack_core::queue::JobState::Done
            | bookrack_core::queue::JobState::Failed
            | bookrack_core::queue::JobState::Cancelled => {
                if let Some(finished_at) = job.finished_at {
                    let candidate = JobOutcomeSummary {
                        job_id: job.id.clone(),
                        kind: job.kind,
                        state: job.state,
                        error: job.error.clone(),
                        finished_at,
                    };
                    last_finished = Some(match last_finished {
                        Some(prev) if prev.finished_at >= candidate.finished_at => prev,
                        _ => candidate,
                    });
                }
            }
        }
    }
    QueueTick {
        current,
        pending,
        running,
        last_finished,
    }
}
