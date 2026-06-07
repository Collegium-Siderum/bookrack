// SPDX-License-Identifier: Apache-2.0

//! Persistent state file for the `bookrack run` REPL's ingest queue,
//! plus the single-puller worker that drains it.
//!
//! The queue lives in a single JSON document under the data root,
//! serialised through serde and rewritten atomically through a sibling
//! temp file + `rename`. The worker is one async task that polls the
//! file under a shared `Mutex`, pulls the next pending job, runs an
//! injected per-job `runner` future, and writes the outcome back. The
//! REPL command dispatcher reuses the same primitives to add / cancel
//! / pause from the foreground.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::sync::broadcast;
use uuid::Uuid;

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

/// File-extension allowlist for the `queue add <dir>` walk. The same
/// list that the legacy `bookrack ingest --recursive` walker uses, so
/// queueing a directory enqueues the same set the operator would have
/// gotten from the standalone command.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["epub", "pdf", "mobi", "azw3", "txt"];

/// Walk `dir` depth-first and collect every regular file whose extension
/// is in [`SUPPORTED_EXTENSIONS`]. Hidden files (those whose name starts
/// with `.`) are skipped. The returned list is sorted by path so
/// re-enqueueing the same directory yields a stable order.
pub fn collect_supported_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .with_context(|| format!("read_dir {}", current.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("entry of {}", current.display()))?;
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let metadata = entry
                .metadata()
                .with_context(|| format!("metadata of {}", path.display()))?;
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            if let Some(ext) = ext
                && SUPPORTED_EXTENSIONS.contains(&ext.as_str())
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Reset every `Running` job to `Pending` so a single-puller worker
/// re-picks it after a crash or `kill -9`. There is no concurrency
/// ambiguity to worry about: only one worker pulls per session, so a
/// row left in `Running` always belongs to a session that did not
/// complete. The ingest crate's noop-if-up-to-date check makes a
/// resumed job cheap when the previous attempt finished its writes.
pub fn crash_recovery_reset(state: &mut QueueState) {
    for job in state.jobs.iter_mut() {
        if matches!(job.state, JobState::Running) {
            job.state = JobState::Pending;
            job.started_at = None;
        }
    }
}

/// Pick the next runnable job. Highest priority first; within the same
/// priority, insertion order. Marks the chosen job `Running` and
/// stamps `started_at`, then returns a clone of the new row so the
/// caller can drive the actual ingest without holding the lock.
///
/// Returns `None` when the queue is paused or no pending job exists.
pub fn pull_pending(state: &mut QueueState) -> Option<QueueJob> {
    if state.paused {
        return None;
    }
    for priority in [Priority::High, Priority::Normal, Priority::Low] {
        for i in 0..state.jobs.len() {
            if state.jobs[i].state == JobState::Pending && state.jobs[i].priority == priority {
                state.jobs[i].state = JobState::Running;
                state.jobs[i].started_at = Some(Utc::now());
                return Some(state.jobs[i].clone());
            }
        }
    }
    None
}

/// Outcome of one job run, applied by [`apply_outcome`].
#[derive(Debug, Clone)]
pub enum JobOutcome {
    /// The ingest call returned `Ok`.
    Done,
    /// The ingest call returned an error; the message is folded into
    /// `QueueJob::error`.
    Failed(String),
}

/// Write `outcome` onto the job identified by `id`. If `id` is no
/// longer in the queue (cancelled and trimmed by a future command),
/// this is a silent no-op so a slow ingest does not crash the worker.
pub fn apply_outcome(state: &mut QueueState, id: &str, outcome: JobOutcome) {
    let Some(job) = state.jobs.iter_mut().find(|j| j.id == id) else {
        return;
    };
    job.finished_at = Some(Utc::now());
    match outcome {
        JobOutcome::Done => {
            job.state = JobState::Done;
            job.error = None;
        }
        JobOutcome::Failed(msg) => {
            job.state = JobState::Failed;
            job.error = Some(msg);
        }
    }
}

/// Append one job per `path` to the queue, all sharing `library`,
/// `priority`, and `force`. Returns the ids of the appended jobs in
/// the order they were inserted.
pub fn enqueue_files(
    state: &mut QueueState,
    paths: &[PathBuf],
    library: &str,
    priority: Priority,
    force: bool,
) -> Vec<String> {
    let mut ids = Vec::with_capacity(paths.len());
    for path in paths {
        let id = Uuid::now_v7().to_string();
        state.jobs.push(QueueJob {
            id: id.clone(),
            library: library.to_string(),
            path: path.clone(),
            priority,
            force,
            state: JobState::Pending,
            queued_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
        });
        ids.push(id);
    }
    ids
}

/// Cancel the unique pending or running job whose id starts with
/// `prefix`. The matched job moves to [`JobState::Cancelled`] and its
/// `finished_at` is stamped.
///
/// Returns the cancelled job's full id on success, or an error if no
/// job (or more than one) matches the prefix. Empty prefixes are
/// rejected so a typo does not cancel the next-in-line by accident.
pub fn cancel_with_prefix(state: &mut QueueState, prefix: &str) -> Result<String> {
    if prefix.is_empty() {
        return Err(anyhow!("queue cancel: id prefix must not be empty"));
    }
    let candidates: Vec<usize> = state
        .jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| {
            j.id.starts_with(prefix) && matches!(j.state, JobState::Pending | JobState::Running)
        })
        .map(|(i, _)| i)
        .collect();
    match candidates.len() {
        0 => Err(anyhow!(
            "queue cancel: no pending or running job matches prefix {prefix:?}"
        )),
        1 => {
            let idx = candidates[0];
            let job = &mut state.jobs[idx];
            job.state = JobState::Cancelled;
            job.finished_at = Some(Utc::now());
            Ok(job.id.clone())
        }
        n => Err(anyhow!(
            "queue cancel: prefix {prefix:?} is ambiguous, matches {n} jobs"
        )),
    }
}

/// Cancel every `Pending` job in one sweep. Running jobs are left
/// alone — they finish or fail naturally; aborting an in-flight
/// ingest mid-stage would risk an inconsistent catalog. Returns the
/// number of jobs cancelled.
pub fn cancel_all_pending(state: &mut QueueState) -> usize {
    let now = Utc::now();
    let mut count = 0usize;
    for job in state.jobs.iter_mut() {
        if matches!(job.state, JobState::Pending) {
            job.state = JobState::Cancelled;
            job.finished_at = Some(now);
            count += 1;
        }
    }
    count
}

/// Render the queue as a fixed-width table, one row per job. Columns:
/// short id (first 8 chars), state, library, file name, queued-at
/// timestamp.
pub fn render_list(state: &QueueState) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    if state.paused {
        out.push_str("queue: PAUSED\n");
    }
    if state.jobs.is_empty() {
        out.push_str("(queue is empty)\n");
        return out;
    }
    let _ = writeln!(
        out,
        "{:<10}  {:<9}  {:<12}  {:<40}  QUEUED",
        "ID", "STATE", "LIBRARY", "FILE",
    );
    for job in &state.jobs {
        let short_id: String = job.id.chars().take(8).collect();
        let state_label = match job.state {
            JobState::Pending => "pending",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        };
        let file = job
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| job.path.display().to_string());
        let queued = job.queued_at.format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(
            out,
            "{:<10}  {:<9}  {:<12}  {:<40}  {queued}",
            short_id, state_label, job.library, file,
        );
    }
    out
}

/// The single-puller worker.
///
/// Crash-recovers on entry (any `Running` job from a prior session is
/// reset to `Pending`, see [`crash_recovery_reset`]), then loops:
/// `select` on the shutdown broadcast or a 200 ms tick, pull the next
/// pending job, drive it through `runner`, write the outcome back,
/// repeat. `runner` is intentionally a closure so the production wiring
/// can route through the registry while tests inject a fake outcome
/// without standing up a real ingest.
///
/// State file writes that fail are logged through `tracing::error`
/// and the loop continues — losing one rewrite is recoverable on the
/// next save, but exiting the worker would strand the queue.
pub async fn worker_loop<R, Fut>(
    state_path: PathBuf,
    state: Arc<Mutex<QueueState>>,
    mut shutdown_rx: broadcast::Receiver<()>,
    runner: R,
) -> Result<()>
where
    R: Fn(QueueJob) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<(), String>> + Send,
{
    // Crash recovery once at startup.
    {
        let mut guard = state.lock().expect("queue state mutex poisoned");
        crash_recovery_reset(&mut guard);
        if let Err(err) = save_atomic(&guard, &state_path) {
            tracing::error!(error = %err, "queue worker: persist after crash recovery");
        }
    }

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }

        let pulled = {
            let mut guard = state.lock().expect("queue state mutex poisoned");
            let job = pull_pending(&mut guard);
            if job.is_some()
                && let Err(err) = save_atomic(&guard, &state_path)
            {
                tracing::error!(error = %err, "queue worker: persist after pull");
            }
            job
        };
        let Some(job) = pulled else {
            continue;
        };

        let outcome = match runner(job.clone()).await {
            Ok(()) => JobOutcome::Done,
            Err(msg) => JobOutcome::Failed(msg),
        };

        announce_outcome(&job, &outcome);

        {
            let mut guard = state.lock().expect("queue state mutex poisoned");
            apply_outcome(&mut guard, &job.id, outcome);
            if let Err(err) = save_atomic(&guard, &state_path) {
                tracing::error!(error = %err, "queue worker: persist after outcome");
            }
        }
    }
}

/// Emit one terminating line per job onto stderr — a job-completion
/// marker that lands on its own line after the long tail of tracing /
/// progress output an ingest produces. The line gives the operator a
/// clean visual cue that the worker has finished; without it, the last
/// terminal row is the trailing progress line and the REPL prompt sits
/// invisibly N rows above, indistinguishable from a hung daemon.
fn announce_outcome(job: &QueueJob, outcome: &JobOutcome) {
    let short: String = job.id.chars().take(8).collect();
    let name = job
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| job.path.display().to_string());
    match outcome {
        JobOutcome::Done => eprintln!("queue: {short} {name} done"),
        JobOutcome::Failed(msg) => eprintln!("queue: {short} {name} failed: {msg}"),
    }
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

    fn job(id: &str, priority: Priority, state: JobState) -> QueueJob {
        QueueJob {
            id: id.to_string(),
            library: "default".to_string(),
            path: PathBuf::from(format!("/tmp/{id}.epub")),
            priority,
            force: false,
            state,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
        }
    }

    #[test]
    fn crash_recovery_resets_running_to_pending() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![
                job("aaaaaaaa", Priority::Normal, JobState::Running),
                job("bbbbbbbb", Priority::Normal, JobState::Pending),
                job("cccccccc", Priority::Normal, JobState::Done),
                job("dddddddd", Priority::Normal, JobState::Failed),
            ],
        };
        // Stamp started_at on the running row so we can prove it is
        // cleared along with the state transition.
        state.jobs[0].started_at = Some(Utc::now());
        crash_recovery_reset(&mut state);
        assert_eq!(state.jobs[0].state, JobState::Pending);
        assert!(state.jobs[0].started_at.is_none());
        assert_eq!(state.jobs[1].state, JobState::Pending);
        assert_eq!(state.jobs[2].state, JobState::Done);
        assert_eq!(state.jobs[3].state, JobState::Failed);
    }

    #[test]
    fn pull_pending_respects_priority_then_insertion_order() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![
                job("aaaa", Priority::Low, JobState::Pending),
                job("bbbb", Priority::Normal, JobState::Pending),
                job("cccc", Priority::High, JobState::Pending),
                job("dddd", Priority::High, JobState::Pending),
                job("eeee", Priority::Normal, JobState::Pending),
            ],
        };
        let first = pull_pending(&mut state).expect("first pull");
        assert_eq!(first.id, "cccc");
        assert_eq!(state.jobs[2].state, JobState::Running);
        let second = pull_pending(&mut state).expect("second pull");
        assert_eq!(second.id, "dddd");
        let third = pull_pending(&mut state).expect("third pull");
        // bbbb was inserted before eeee at the same priority.
        assert_eq!(third.id, "bbbb");
    }

    #[test]
    fn paused_state_blocks_pull() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: true,
            jobs: vec![job("aaaa", Priority::High, JobState::Pending)],
        };
        assert!(pull_pending(&mut state).is_none());
        assert_eq!(state.jobs[0].state, JobState::Pending);
        state.paused = false;
        assert!(pull_pending(&mut state).is_some());
    }

    #[test]
    fn cancel_with_unique_prefix_marks_cancelled() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![
                job("aaaa1111", Priority::Normal, JobState::Pending),
                job("bbbb2222", Priority::Normal, JobState::Pending),
            ],
        };
        let id = cancel_with_prefix(&mut state, "aaaa").unwrap();
        assert_eq!(id, "aaaa1111");
        assert_eq!(state.jobs[0].state, JobState::Cancelled);
        assert!(state.jobs[0].finished_at.is_some());
        assert_eq!(state.jobs[1].state, JobState::Pending);
    }

    #[test]
    fn cancel_with_ambiguous_prefix_errors_without_mutation() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![
                job("abcd1111", Priority::Normal, JobState::Pending),
                job("abcd2222", Priority::Normal, JobState::Pending),
            ],
        };
        let err = cancel_with_prefix(&mut state, "abcd").unwrap_err();
        assert!(format!("{err}").contains("ambiguous"));
        assert_eq!(state.jobs[0].state, JobState::Pending);
        assert_eq!(state.jobs[1].state, JobState::Pending);
    }

    #[test]
    fn cancel_skips_completed_states() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![
                job("aaaa1111", Priority::Normal, JobState::Done),
                job("aaaa2222", Priority::Normal, JobState::Pending),
            ],
        };
        let id = cancel_with_prefix(&mut state, "aaaa").unwrap();
        assert_eq!(id, "aaaa2222");
    }

    #[test]
    fn cancel_all_pending_skips_running() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![
                job("aaaa", Priority::Normal, JobState::Pending),
                job("bbbb", Priority::Normal, JobState::Running),
                job("cccc", Priority::Normal, JobState::Pending),
            ],
        };
        let n = cancel_all_pending(&mut state);
        assert_eq!(n, 2);
        assert_eq!(state.jobs[0].state, JobState::Cancelled);
        assert_eq!(state.jobs[1].state, JobState::Running);
        assert_eq!(state.jobs[2].state, JobState::Cancelled);
    }

    #[test]
    fn collect_supported_files_walks_subdirs_and_skips_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.epub"), b"e").unwrap();
        std::fs::write(root.join("b.pdf"), b"p").unwrap();
        std::fs::write(root.join("c.unsupported"), b"x").unwrap();
        std::fs::write(root.join(".hidden.epub"), b"h").unwrap();
        std::fs::write(root.join("sub/d.txt"), b"t").unwrap();
        let files = collect_supported_files(root).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.epub", "b.pdf", "d.txt"]);
    }

    #[test]
    fn enqueue_files_appends_one_job_per_path() {
        let mut state = QueueState::default();
        let paths = vec![
            PathBuf::from("/tmp/a.epub"),
            PathBuf::from("/tmp/b.pdf"),
            PathBuf::from("/tmp/c.txt"),
        ];
        let ids = enqueue_files(&mut state, &paths, "books", Priority::High, true);
        assert_eq!(ids.len(), 3);
        assert_eq!(state.jobs.len(), 3);
        for (i, job) in state.jobs.iter().enumerate() {
            assert_eq!(job.id, ids[i]);
            assert_eq!(job.library, "books");
            assert_eq!(job.priority, Priority::High);
            assert!(job.force);
            assert_eq!(job.state, JobState::Pending);
            assert_eq!(job.path, paths[i]);
        }
    }

    #[test]
    fn apply_outcome_done_clears_error_and_stamps_finished_at() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![job("xx", Priority::Normal, JobState::Running)],
        };
        state.jobs[0].error = Some("stale".to_string());
        apply_outcome(&mut state, "xx", JobOutcome::Done);
        assert_eq!(state.jobs[0].state, JobState::Done);
        assert!(state.jobs[0].error.is_none());
        assert!(state.jobs[0].finished_at.is_some());
    }

    #[test]
    fn apply_outcome_failed_records_error_message() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![job("xx", Priority::Normal, JobState::Running)],
        };
        apply_outcome(&mut state, "xx", JobOutcome::Failed("boom".to_string()));
        assert_eq!(state.jobs[0].state, JobState::Failed);
        assert_eq!(state.jobs[0].error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn worker_loop_exits_on_shutdown_signal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let state = Arc::new(Mutex::new(QueueState::default()));
        let (tx, rx) = broadcast::channel::<()>(2);
        let handle = tokio::spawn(worker_loop(path, Arc::clone(&state), rx, |_| async {
            Ok::<(), String>(())
        }));
        tx.send(()).expect("send shutdown");
        let res = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("worker did not exit within 2 s");
        res.expect("join worker").expect("worker_loop result");
    }

    #[tokio::test]
    async fn worker_loop_picks_pending_then_marks_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut initial = QueueState::default();
        let _ids = enqueue_files(
            &mut initial,
            &[PathBuf::from("/tmp/only.epub")],
            "default",
            Priority::Normal,
            false,
        );
        let state = Arc::new(Mutex::new(initial));
        let (tx, rx) = broadcast::channel::<()>(2);
        let handle = {
            let state = Arc::clone(&state);
            tokio::spawn(worker_loop(path.clone(), state, rx, |_job| async {
                Ok::<(), String>(())
            }))
        };
        // The worker's tick is 200 ms; wait long enough for it to pull,
        // resolve the runner, and persist the outcome.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let done = {
                let guard = state.lock().unwrap();
                guard.jobs.iter().any(|j| j.state == JobState::Done)
            };
            if done {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not mark the job Done within 3 s"
            );
        }
        let snapshot = state.lock().unwrap().clone();
        assert_eq!(snapshot.jobs.len(), 1);
        assert_eq!(snapshot.jobs[0].state, JobState::Done);
        assert!(snapshot.jobs[0].finished_at.is_some());
        tx.send(()).expect("send shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
