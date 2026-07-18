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

/// Adapter to the uniform `(params, ctx) -> Result<Value, RpcError>`
/// signature consumed by the dispatch macro.
pub fn daemon_version_rpc(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    Ok(daemon_version(ctx))
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
        // `false` on a headless entry point without a queue worker;
        // clients that would enqueue work (e.g. `index-profile apply`)
        // check this up front instead of failing on their first call.
        "queue_worker_enabled": ctx.queue_worker_enabled,
        // Identity of the served library. `library` is the registry
        // name and `null` when the data root was selected directly by
        // path — a normal state, matching the lock file's omitted
        // `library_name=` line, not the fabricated fallback in
        // `ctx.library_name`. Single-library snapshot fields: a daemon
        // serves exactly one library today.
        "library": ctx.info_context.library_name,
        "data_dir": ctx.info_context.data_dir,
    })
}

pub async fn doctor_gather(ctx: &MethodContext) -> Value {
    let report = doctor::gather_with(&ctx.selection, ctx.rerank_supervisor.as_deref()).await;
    serde_json::to_value(report).unwrap_or(Value::Null)
}

pub fn status_rpc(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    Ok(status(ctx))
}

pub async fn doctor_gather_rpc(
    _params: &Option<Value>,
    ctx: &MethodContext,
) -> Result<Value, RpcError> {
    Ok(doctor_gather(ctx).await)
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
    let summary = derive_queue_summary(&state);
    let mut jobs = state.jobs.clone();
    if let Some(limit) = parsed.limit {
        jobs.truncate(limit as usize);
    }
    Ok(json!({
        "schema_version": state.schema_version,
        "paused": state.paused,
        "summary": summary,
        "jobs": jobs,
    }))
}

/// Counts jobs by state across the whole queue, independent of any
/// `limit` applied to the returned `jobs` array, so clients get exact
/// totals without regrouping the truncated list.
fn derive_queue_summary(state: &bookrack_core::queue::QueueState) -> Value {
    use bookrack_core::queue::JobState;
    let mut pending = 0u32;
    let mut running = 0u32;
    let mut done = 0u32;
    let mut skipped_duplicate = 0u32;
    let mut needs_ocr = 0u32;
    let mut failed = 0u32;
    let mut cancelled = 0u32;
    for job in &state.jobs {
        match job.state {
            JobState::Pending => pending += 1,
            JobState::Running => running += 1,
            JobState::Done => done += 1,
            JobState::SkippedDuplicate => skipped_duplicate += 1,
            JobState::NeedsOcr => needs_ocr += 1,
            JobState::Failed => failed += 1,
            JobState::Cancelled => cancelled += 1,
        }
    }
    json!({
        "pending": pending,
        "running": running,
        "done": done,
        "skipped_duplicate": skipped_duplicate,
        "needs_ocr": needs_ocr,
        "failed": failed,
        "cancelled": cancelled,
        "total": state.jobs.len() as u32,
    })
}

pub fn library_list(ctx: &MethodContext) -> Result<Value, RpcError> {
    let summaries = ctx
        .registry
        .list()
        .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("registry list failed: {e}")))?;
    Ok(library_list_value(summaries))
}

pub fn library_list_rpc(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    library_list(ctx)
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
            | bookrack_core::queue::JobState::SkippedDuplicate
            | bookrack_core::queue::JobState::NeedsOcr
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use bookrack_config::{Config, LibrarySelection};
    use bookrack_core::queue::QueueState;
    use bookrack_embed::OllamaEmbedClient;
    use bookrack_obs::stream::LogStreamHandle;
    use bookrack_ops::reads::info::LibraryInfoContext;
    use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
    use bookrack_ops::{Caller, Ops};
    use tokio::sync::{Mutex as TokioMutex, Notify, broadcast};

    use super::*;
    use crate::control::events::{DaemonState, DaemonStateFlag, EventStreamHandle};
    use crate::control::plan_registry::PlanRegistry;

    /// Build a [`MethodContext`] over a catalog-only ops handle rooted
    /// at `dir`, so no embedder probe runs. `library_name` is the
    /// registry name of the served library, `None` for a path-selected
    /// root.
    fn method_context(dir: &Path, library_name: Option<&str>) -> MethodContext {
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            dir.join("corpus.db"),
            dir.join("catalog.db"),
            &dir.join("lancedb"),
            dir.join("books"),
            dir.join("backup"),
            Caller::cli(),
        );
        let handle = LibraryHandle::new(library_name.unwrap_or("default"), ops);
        let state = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let (shutdown_tx, _) = broadcast::channel(8);
        MethodContext {
            cfg: Arc::new(Config::new(
                dir.to_path_buf(),
                "http://127.0.0.1:11434".to_string(),
            )),
            registry: LibraryRegistry::single(handle),
            info_context: LibraryInfoContext {
                data_dir: dir.display().to_string(),
                library_name: library_name.map(str::to_string),
                resolution_source: "explicit".to_string(),
                shadowed_default: None,
                library_identification: None,
                ollama_url: "http://127.0.0.1:11434".to_string(),
                embed_model_configured: "test-model".to_string(),
                mcp_addr: String::new(),
            },
            queue_state: Arc::new(Mutex::new(QueueState::default())),
            queue_state_path: dir.join(".bookrack-queue.json"),
            event_stream: EventStreamHandle::new(8, state),
            write_guard: Arc::new(TokioMutex::new(())),
            shutdown_tx,
            started_at_rfc3339: "2026-01-01T00:00:00Z".to_string(),
            selection: LibrarySelection::default(),
            library_name: library_name.unwrap_or("default").to_string(),
            mcp_tools: Arc::new(Vec::new()),
            queue_worker_enabled: false,
            tray_focus_signal: Arc::new(Notify::new()),
            rerank_supervisor: None,
            queue_paused: Arc::new(AtomicBool::new(false)),
            log_stream: LogStreamHandle::new(8, 8),
            plan_registry: Arc::new(PlanRegistry::new()),
        }
    }

    #[test]
    fn status_reports_the_registry_name_for_a_named_library() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = method_context(dir.path(), Some("main"));
        let value = status(&ctx);
        assert_eq!(value["library"], "main");
        assert_eq!(value["data_dir"], dir.path().display().to_string());
    }

    #[test]
    fn status_reports_null_library_for_a_path_selected_root() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = method_context(dir.path(), None);
        let value = status(&ctx);
        assert!(
            value["library"].is_null(),
            "a path-selected root has no registry name: {value}"
        );
        assert_eq!(value["data_dir"], dir.path().display().to_string());
    }

    #[test]
    fn status_keeps_the_queue_and_state_fields() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = method_context(dir.path(), None);
        let value = status(&ctx);
        for key in [
            "state",
            "queue_pending",
            "queue_running",
            "queue_worker_enabled",
        ] {
            assert!(value.get(key).is_some(), "missing key {key}: {value}");
        }
    }
}
