// SPDX-License-Identifier: Apache-2.0

//! `ingest.submit` and `ingest.cancel` JSON-RPC handlers.
//!
//! Neither method touches the catalog, corpus, or vector store
//! directly — both only mutate the on-disk queue document. The actual
//! ingest runs out of the worker loop, where the daemon-state flag and
//! the broadcast notifications already fire. As a result these
//! handlers do not take the write mutex and do not transition the
//! daemon into [`crate::control::events::DaemonState::Writing`].
//! They still broadcast a [`crate::control::events::QueueTick`] after
//! persisting so connected clients update their view immediately.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::MethodContext;
use crate::control::events::{Event, JobOutcomeSummary, QueueTick};
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::queue::{self, JobState, Priority};

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct IngestSubmitParams {
    #[cfg_attr(test, ts(type = "Array<string>"))]
    paths: Vec<PathBuf>,
    #[serde(default)]
    library: Option<String>,
    #[serde(default)]
    priority: Option<PriorityRepr>,
    #[serde(default)]
    force: bool,
    /// When `true`, every directory in `paths` is walked depth-first
    /// and expanded to the set of supported-format files inside it
    /// before enqueueing. Files passed directly are still enqueued
    /// verbatim. When `false` (default), directory paths reach the
    /// queue worker as-is, which treats them as ingest failures.
    #[serde(default)]
    recursive: bool,
    /// When `true`, the worker parks each book at STRUCTURE if the
    /// audit verdict is `needs_work`, skipping CHUNK and EMBED until
    /// a curator drives the book past the metadata gate via
    /// `metadata.approve` or `metadata.advance`. Off by default; the
    /// audit remains advisory.
    #[serde(default)]
    hold_for_metadata: bool,
    /// Optional book-side audit profile name. When set, every enqueued
    /// job carries this override and the worker reloads the named
    /// built-in (`default` / `trust-source` / `strict`) before running
    /// the ingest. When unset, the daemon's startup profile applies.
    #[serde(default)]
    audit_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
#[serde(rename_all = "lowercase")]
pub enum PriorityRepr {
    Low,
    Normal,
    High,
}

impl PriorityRepr {
    pub(super) fn into_priority(self) -> Priority {
        match self {
            PriorityRepr::Low => Priority::Low,
            PriorityRepr::Normal => Priority::Normal,
            PriorityRepr::High => Priority::High,
        }
    }
}

pub async fn submit(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: IngestSubmitParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid ingest.submit params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "missing ingest.submit params",
            ));
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
    let expanded: Vec<PathBuf> = if parsed.recursive {
        let mut out = Vec::new();
        for path in &parsed.paths {
            if path.is_dir() {
                let mut found = queue::collect_supported_files(path).map_err(|e| {
                    RpcError::new(INTERNAL_ERROR, format!("walk {}: {e}", path.display()))
                })?;
                out.append(&mut found);
            } else {
                out.push(path.clone());
            }
        }
        out
    } else {
        parsed.paths.clone()
    };
    if expanded.is_empty() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "no supported files found in submitted paths",
        ));
    }
    let ids = {
        let mut guard = ctx
            .queue_state
            .lock()
            .map_err(|_| RpcError::new(INTERNAL_ERROR, "queue state lock poisoned"))?;
        let ids = queue::enqueue_files(
            &mut guard,
            &expanded,
            &library,
            bookrack_core::ItemKind::Book,
            priority,
            parsed.force,
            parsed.hold_for_metadata,
            parsed.audit_profile.clone(),
        );
        queue::save_atomic(&guard, &ctx.queue_state_path)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("persist queue state: {e}")))?;
        let tick = derive_tick(&guard, None);
        ctx.event_stream.publish(Event::QueueTick(tick));
        ids
    };
    Ok(json!({ "job_ids": ids }))
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct IngestCancelParams {
    job_id: String,
}

pub async fn cancel(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: IngestCancelParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid ingest.cancel params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "missing ingest.cancel params",
            ));
        }
    };
    let outcome = {
        let mut guard = ctx
            .queue_state
            .lock()
            .map_err(|_| RpcError::new(INTERNAL_ERROR, "queue state lock poisoned"))?;
        match queue::cancel_with_prefix(&mut guard, &parsed.job_id) {
            Ok(_) => {
                queue::save_atomic(&guard, &ctx.queue_state_path).map_err(|e| {
                    RpcError::new(INTERNAL_ERROR, format!("persist queue state: {e}"))
                })?;
                let tick = derive_tick(&guard, None);
                ctx.event_stream.publish(Event::QueueTick(tick));
                Ok(true)
            }
            Err(err) => Err(err),
        }
    };
    match outcome {
        Ok(_) => Ok(json!({ "ok": true })),
        Err(err) => Err(RpcError::new(
            crate::control::jsonrpc::JOB_NOT_FOUND,
            format!("ingest.cancel: {err}"),
        )),
    }
}

pub(super) fn derive_tick(
    state: &crate::queue::QueueState,
    last_finished: Option<JobOutcomeSummary>,
) -> QueueTick {
    let mut pending = 0u32;
    let mut running = 0u32;
    let mut current = None;
    for job in &state.jobs {
        match job.state {
            JobState::Pending => pending += 1,
            JobState::Running => {
                running += 1;
                if current.is_none() {
                    current = Some(job.id.clone());
                }
            }
            _ => {}
        }
    }
    QueueTick {
        current,
        pending,
        running,
        last_finished,
    }
}
