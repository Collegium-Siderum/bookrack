// SPDX-License-Identifier: Apache-2.0

//! Persistent ingest queue value types.
//!
//! The queue document is owned by `bookrack-cli`'s REPL worker and
//! its readers — the file format, atomic write, walker, and worker
//! loop all live there. This module carries only the pure serde
//! types that cross crate boundaries: the MCP `session.queue_status`
//! tool reads the same `QueueState` snapshot the CLI mutates, and
//! both ends ship the same `Priority` / `JobState` / `QueueJob` over
//! the wire without duplicating their definitions.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ItemKind;

/// Schema version embedded in the persisted document. Bumped whenever
/// any field shape, enum variant, or invariant changes.
pub const QUEUE_SCHEMA_VERSION: u32 = 2;

/// Pull order hint for the worker. The first pending job at the
/// highest priority is picked next.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

/// Lifecycle state of a queued job.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One row in the persistent queue.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QueueJob {
    /// UUIDv7 string. Prefix matching is a plain `str::starts_with`.
    pub id: String,
    /// Library name the job runs against, as registered in the
    /// `LibraryRegistry`.
    pub library: String,
    /// Source file to ingest. Resolved when the job was enqueued; the
    /// worker does not re-resolve relative paths.
    pub path: PathBuf,
    /// Scheduling hint for the worker pull order.
    pub priority: Priority,
    /// Force a fresh ingest even when the source's noop-if-up-to-date
    /// check would otherwise short-circuit.
    pub force: bool,
    /// Which pipeline owns this job: `Book` is dispatched to the ingest
    /// pipeline, `Paper` to the glean pipeline. Defaults to `Book`, so
    /// a v1 queue document (no `kind` field) loads as a book queue.
    #[serde(default)]
    pub kind: ItemKind,
    /// Current lifecycle state.
    pub state: JobState,
    /// When the job entered the queue.
    pub queued_at: DateTime<Utc>,
    /// When the worker transitioned this job to `Running`.
    pub started_at: Option<DateTime<Utc>>,
    /// When the worker transitioned this job to `Done`, `Failed`, or
    /// `Cancelled`.
    pub finished_at: Option<DateTime<Utc>>,
    /// Failure message recorded when `state == Failed`.
    pub error: Option<String>,
}

/// The full document persisted to disk.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QueueState {
    /// Schema revision, currently [`QUEUE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// When set, the worker idles instead of pulling pending jobs.
    pub paused: bool,
    /// Every job, in insertion order.
    pub jobs: Vec<QueueJob>,
}

impl Default for QueueState {
    fn default() -> Self {
        QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_job_without_kind_loads_as_book() {
        // A v1-shaped document persisted before [`QUEUE_SCHEMA_VERSION`]
        // grew the `kind` field must still deserialize cleanly, with
        // every job defaulting to the book pipeline.
        let v1 = r#"{
            "id": "0",
            "library": "default",
            "path": "/tmp/example.epub",
            "priority": "normal",
            "force": false,
            "state": "pending",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": null,
            "finished_at": null,
            "error": null
        }"#;
        let job: QueueJob = serde_json::from_str(v1).expect("deserialize v1");
        assert_eq!(job.kind, ItemKind::Book);
    }

    #[test]
    fn queue_job_round_trips_kind() {
        let job = QueueJob {
            id: "0".to_string(),
            library: "default".to_string(),
            path: "/tmp/example.pdf".into(),
            kind: ItemKind::Paper,
            priority: Priority::Normal,
            force: false,
            state: JobState::Pending,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
        };
        let json = serde_json::to_value(&job).expect("serialize");
        assert_eq!(json["kind"], "paper");
        let back: QueueJob = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.kind, ItemKind::Paper);
        assert_eq!(back, job);
    }
}
