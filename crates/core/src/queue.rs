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

/// Schema version embedded in the persisted document. Bumped whenever
/// any field shape, enum variant, or invariant changes.
pub const QUEUE_SCHEMA_VERSION: u32 = 1;

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
