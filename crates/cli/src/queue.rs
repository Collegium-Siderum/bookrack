// SPDX-License-Identifier: Apache-2.0

//! Persistent state file for the `bookrack run` REPL's ingest queue.
//!
//! The queue lives in a single JSON document under the data root,
//! serialised through serde and rewritten atomically through a sibling
//! temp file + `rename`. This module owns only the on-disk shape and
//! the read/write primitives; the worker loop and the REPL command
//! parser live elsewhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

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

/// Read the queue state at `path`. A missing file deserialises to the
/// default state so a freshly initialised data root just works.
pub fn load(path: &Path) -> Result<QueueState> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse queue state at {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(QueueState::default()),
        Err(e) => Err(e).with_context(|| format!("read queue state at {}", path.display())),
    }
}

/// Write `state` to `path` atomically: a sibling temp file is written
/// and fsynced, then renamed over the destination. A crash mid-write
/// leaves either the previous document or no document at all, never a
/// truncated one.
pub fn save_atomic(state: &QueueState, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory {}", parent.display()))?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)
        .with_context(|| format!("open temp file under {}", parent.display()))?;
    serde_json::to_writer_pretty(tmp.as_file_mut(), state)
        .with_context(|| format!("serialise queue state for {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("fsync queue state temp under {}", parent.display()))?;
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!(e.error))
        .with_context(|| format!("persist queue state to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_job() -> QueueJob {
        QueueJob {
            id: "01900000-0000-7000-8000-000000000001".to_string(),
            library: "default".to_string(),
            path: PathBuf::from("/tmp/example.epub"),
            priority: Priority::Normal,
            force: false,
            state: JobState::Pending,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
        }
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let state = load(&path).unwrap();
        assert_eq!(state, QueueState::default());
        assert_eq!(state.schema_version, QUEUE_SCHEMA_VERSION);
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut state = QueueState::default();
        state.jobs.push(sample_job());
        save_atomic(&state, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn save_atomic_does_not_leave_temp_on_failure() {
        // The parent directory does not exist and create_dir_all
        // cannot create it because a regular file sits in the path.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"file-not-dir").unwrap();
        let path = blocker.join("nested").join("queue.json");
        let err = save_atomic(&QueueState::default(), &path);
        assert!(err.is_err());
        // The blocker file is still a regular file; the directory tree
        // never came into existence and no sibling temp was left.
        assert!(blocker.is_file());
        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != blocker)
            .collect();
        assert!(stray.is_empty(), "stray entries: {stray:?}");
    }

    #[test]
    fn schema_version_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: true,
            jobs: vec![sample_job()],
        };
        save_atomic(&state, &path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(&format!("\"schema_version\": {QUEUE_SCHEMA_VERSION}")),
            "schema_version missing from serialised form: {raw}"
        );
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.schema_version, QUEUE_SCHEMA_VERSION);
        assert!(loaded.paused);
    }
}
