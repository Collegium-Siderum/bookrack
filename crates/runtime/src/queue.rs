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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use eyre::{Context, Result, eyre};
use tempfile::NamedTempFile;
use tokio::sync::broadcast;
use uuid::Uuid;

use bookrack_core::ItemKind;
pub use bookrack_core::queue::{
    IntakeOcrInfo, JobState, Priority, QUEUE_SCHEMA_VERSION, QueueJob, QueueState,
};

use crate::control::events::{Event, EventStreamHandle, JobOutcomeSummary, QueueTick};

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
        .map_err(|e| eyre::eyre!(e.error))
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

/// Success payload the runner returns to the worker loop.
///
/// The worker collapses this into a [`JobOutcome`]. `needs_ocr == true`
/// means the source has no usable text layer and was registered as a
/// `needs_ocr` intake anchor; it maps to [`JobOutcome::NeedsOcr`].
/// Otherwise `no_op == true` means the ingest short-circuited on an
/// already-registered source with up-to-date stamps «catalog unchanged»
/// and maps to [`JobOutcome::SkippedDuplicate`]; a plain success maps to
/// [`JobOutcome::Done`]. `intake_id` is the catalog row the source
/// resolved to — the existing entry on the skip branch, the anchor on
/// the needs-OCR branch — forwarded onto `QueueJob::merged_into` so
/// `queue list` can point the operator at the intake. Paper and
/// reference pipelines that do not surface an intake id set it to
/// `None`; the skip and needs-OCR branches are book-only in practice.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobSuccess {
    pub no_op: bool,
    pub needs_ocr: bool,
    pub intake_id: Option<i64>,
}

impl JobSuccess {
    pub fn done() -> Self {
        Self {
            no_op: false,
            needs_ocr: false,
            intake_id: None,
        }
    }
}

/// Outcome of one job run, applied by [`apply_outcome`].
#[derive(Debug, Clone)]
pub enum JobOutcome {
    /// The ingest call returned `Ok` and actually wrote to the catalog.
    Done,
    /// The ingest call returned `Ok` from the noop-if-up-to-date fast
    /// path: the source was already on file and every stamp matched,
    /// so no catalog write happened. The wrapped intake id points at
    /// the existing catalog row.
    SkippedDuplicate { intake_id: i64 },
    /// The ingest rejected the source as image-only and registered a
    /// `needs_ocr` intake anchor for a later OCR pass. The wrapped
    /// intake id points at that anchor. A non-failure terminal: no book
    /// content was written, but the job did what it could.
    NeedsOcr { intake_id: i64 },
    /// The ingest call returned an error; the message is folded into
    /// `QueueJob::error`.
    Failed(String),
}

/// Error returned by the worker's injected runner, classified by
/// blast radius.
#[derive(Debug, Clone)]
pub enum JobError {
    /// The book itself failed; the worker records the failure and
    /// moves on to the next pending job.
    Book(String),
    /// The daemon process cannot currently serve any job (e.g. its
    /// file descriptors are exhausted); the worker records the
    /// failure and pauses the queue instead of burning the remaining
    /// pending jobs against a store that fails every open.
    Process(String),
}

impl JobError {
    fn into_message(self) -> String {
        match self {
            JobError::Book(msg) | JobError::Process(msg) => msg,
        }
    }
}

/// Classify a failed ingest into a [`JobError`]. File-descriptor
/// exhaustion anywhere in the chain is process-level: every later job
/// would fail the same way until the process gets descriptors back.
pub fn classify_ingest_error(err: &eyre::Report) -> JobError {
    let message = format!("ingest: {err:#}");
    if is_fd_exhaustion(err) {
        JobError::Process(message)
    } else {
        JobError::Book(message)
    }
}

/// If `err` carries an [`IngestError::NeedsOcr`] anywhere in its chain,
/// return the registered anchor intake id. The ingest layer wraps its
/// typed error with `.context(...)` before it reaches the worker, so
/// the variant sits as a source on the chain rather than at the top;
/// walk the chain and downcast, exactly as the control-plane error map
/// does. `None` for every other error, which then flows through
/// [`classify_ingest_error`] as usual.
pub fn ingest_needs_ocr(err: &eyre::Report) -> Option<i64> {
    err.chain().find_map(
        |cause| match cause.downcast_ref::<bookrack_ingest::IngestError>() {
            Some(bookrack_ingest::IngestError::NeedsOcr { intake_id, .. }) => Some(*intake_id),
            _ => None,
        },
    )
}

fn is_fd_exhaustion(err: &eyre::Report) -> bool {
    let errno_hit = err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(errno_is_fd_exhaustion)
    });
    // LanceDB flattens some OS errors into message strings before
    // they cross its API boundary; match the strerror text those
    // messages carry as a fallback.
    errno_hit
        || err
            .chain()
            .any(|cause| cause.to_string().contains("Too many open files"))
}

#[cfg(unix)]
fn errno_is_fd_exhaustion(code: i32) -> bool {
    code == rustix::io::Errno::MFILE.raw_os_error()
        || code == rustix::io::Errno::NFILE.raw_os_error()
}

#[cfg(not(unix))]
fn errno_is_fd_exhaustion(_code: i32) -> bool {
    false
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
            job.merged_into = None;
        }
        JobOutcome::SkippedDuplicate { intake_id } => {
            job.state = JobState::SkippedDuplicate;
            job.error = None;
            job.merged_into = Some(intake_id);
        }
        JobOutcome::NeedsOcr { intake_id } => {
            job.state = JobState::NeedsOcr;
            job.error = None;
            job.merged_into = Some(intake_id);
        }
        JobOutcome::Failed(msg) => {
            job.state = JobState::Failed;
            job.error = Some(msg);
            job.merged_into = None;
        }
    }
}

/// Append one job per `path` to the queue, all sharing `library`,
/// `kind`, `priority`, `force`, `hold_for_metadata`, and
/// `book_audit_profile`. Returns the ids of the appended jobs in the
/// order they were inserted.
///
/// `book_audit_profile` overrides the book-side audit profile used by
/// the worker for the appended jobs. It is named for the book pipeline
/// because the paper pipeline (glean) consults its own paper audit
/// profile through `glean_params_template`; callers enqueueing
/// paper-kind jobs must pass `None` here, and any future paper-side
/// override needs a separate field on the job.
// The argument list mirrors the shape of `QueueJob` itself; collapsing
// the seven scalars into an options struct would only add an
// intermediate type that callers immediately destructure.
#[allow(clippy::too_many_arguments)]
pub fn enqueue_files(
    state: &mut QueueState,
    paths: &[PathBuf],
    library: &str,
    kind: ItemKind,
    priority: Priority,
    force: bool,
    hold_for_metadata: bool,
    book_audit_profile: Option<String>,
) -> Vec<String> {
    let mut ids = Vec::with_capacity(paths.len());
    for path in paths {
        let id = Uuid::now_v7().to_string();
        state.jobs.push(QueueJob {
            id: id.clone(),
            library: library.to_string(),
            path: path.clone(),
            kind,
            priority,
            force,
            hold_for_metadata,
            intake_ocr: None,
            audit_profile: book_audit_profile.clone(),
            state: JobState::Pending,
            queued_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
        });
        ids.push(id);
    }
    ids
}

/// Append one OCR-intake job to the queue. `path` is the OCR markdown
/// product; `info` carries the source PDF anchor and the runtime knobs
/// (`expected_pages`, `allow_partial`) the OCR ingest path consumes.
/// Returns the appended job's id.
///
/// OCR intakes are dispatched to the book pipeline — the worker routes
/// on `intake_ocr.is_some()`, then calls the OCR ingest path with the
/// stored markdown + PDF pair. The job's `kind` therefore stays
/// `ItemKind::Book`; a queue listing reads it as a book job.
// Same trade-off as `enqueue_files`: the scalars correspond one-to-one
// to `QueueJob` fields and an options struct would just be a thin
// destructure target at every caller.
#[allow(clippy::too_many_arguments)]
pub fn enqueue_ocr_intake(
    state: &mut QueueState,
    path: PathBuf,
    info: IntakeOcrInfo,
    library: &str,
    priority: Priority,
    force: bool,
    hold_for_metadata: bool,
    book_audit_profile: Option<String>,
) -> String {
    let id = Uuid::now_v7().to_string();
    state.jobs.push(QueueJob {
        id: id.clone(),
        library: library.to_string(),
        path,
        kind: ItemKind::Book,
        priority,
        force,
        hold_for_metadata,
        intake_ocr: Some(info),
        audit_profile: book_audit_profile,
        state: JobState::Pending,
        queued_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        merged_into: None,
    });
    id
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
        return Err(eyre!("queue cancel: id prefix must not be empty"));
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
        0 => Err(eyre!(
            "queue cancel: no pending or running job matches prefix {prefix:?}"
        )),
        1 => {
            let idx = candidates[0];
            let job = &mut state.jobs[idx];
            job.state = JobState::Cancelled;
            job.finished_at = Some(Utc::now());
            Ok(job.id.clone())
        }
        n => Err(eyre!(
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
        "{:<10}  {:<17}  {:<12}  {:<40}  QUEUED",
        "ID", "STATE", "LIBRARY", "FILE",
    );
    for job in &state.jobs {
        let short_id: String = job.id.chars().take(8).collect();
        let state_label = job.state.as_wire_str();
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

pub async fn worker_loop<R, Fut>(
    state_path: PathBuf,
    state: Arc<Mutex<QueueState>>,
    mut shutdown_rx: broadcast::Receiver<()>,
    runner: R,
    events: EventStreamHandle,
    paused: Arc<AtomicBool>,
) -> Result<()>
where
    R: Fn(QueueJob) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<JobSuccess, JobError>> + Send,
{
    // Crash recovery once at startup.
    {
        let mut guard = state.lock().expect("queue state mutex poisoned");
        crash_recovery_reset(&mut guard);
        if let Err(err) = save_atomic(&guard, &state_path) {
            tracing::error!(error = %err, "queue worker: persist after crash recovery");
        }
        let tick = derive_tick(&guard, None);
        events.publish(Event::QueueTick(tick));
    }

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }

        if paused.load(Ordering::Acquire) {
            continue;
        }

        let pulled = {
            let mut guard = state.lock().expect("queue state mutex poisoned");
            let job = pull_pending(&mut guard);
            if job.is_some() {
                if let Err(err) = save_atomic(&guard, &state_path) {
                    tracing::error!(error = %err, "queue worker: persist after pull");
                }
                let tick = derive_tick(&guard, None);
                events.publish(Event::QueueTick(tick));
            }
            job
        };
        let Some(job) = pulled else {
            continue;
        };

        // Hold the working guard across the runner so the daemon
        // state reads `working` for exactly the execution span; the
        // outcome-persist block below runs after the guard drops.
        let working = events.job_guard();
        let run_result = runner(job.clone()).await;
        drop(working);
        let (outcome, pause_queue) = match run_result {
            Ok(JobSuccess {
                needs_ocr: true,
                intake_id: Some(intake_id),
                ..
            }) => (JobOutcome::NeedsOcr { intake_id }, false),
            Ok(JobSuccess {
                needs_ocr: true, ..
            }) => {
                // A needs-OCR success without an anchor id. The book
                // path always registers the anchor before surfacing
                // this, so the arm only guards a future runner that
                // omits the id; fall back to Done rather than a
                // needs_ocr terminal with a dangling `merged_into`.
                (JobOutcome::Done, false)
            }
            Ok(JobSuccess {
                no_op: true,
                intake_id: Some(intake_id),
                ..
            }) => (JobOutcome::SkippedDuplicate { intake_id }, false),
            Ok(JobSuccess { no_op: true, .. }) => {
                // A pipeline claimed the noop-if-up-to-date fast path
                // without surfacing an intake id — record it as a
                // regular Done so operators do not see a skip terminal
                // with a dangling `merged_into = null`. The
                // book-ingest path always produces an id on this
                // branch; this arm is a defensive fallback for the
                // paper/reference pipelines and any future runner
                // that omits the id.
                (JobOutcome::Done, false)
            }
            Ok(JobSuccess { no_op: false, .. }) => (JobOutcome::Done, false),
            Err(err) => {
                let pause = matches!(err, JobError::Process(_));
                let message = err.into_message();
                if pause {
                    tracing::error!(
                        job = %job.id,
                        error = %message,
                        "process-level failure; pausing ingest queue",
                    );
                }
                (JobOutcome::Failed(message), pause)
            }
        };

        announce_outcome(&job, &outcome);

        {
            let mut guard = state.lock().expect("queue state mutex poisoned");
            if pause_queue {
                guard.paused = true;
            }
            apply_outcome(&mut guard, &job.id, outcome);
            let save_result = save_atomic(&guard, &state_path);
            if let Err(err) = &save_result {
                tracing::error!(error = %err, "queue worker: persist after outcome");
            }
            if pause_queue {
                if save_result.is_ok() {
                    paused.store(true, Ordering::Release);
                } else {
                    // Persist failed: keep the in-memory pause flag in
                    // sync with what is on disk so a restart and the
                    // running worker agree.
                    guard.paused = false;
                }
            }
            let last_finished = guard
                .jobs
                .iter()
                .find(|j| j.id == job.id)
                .map(summarize_outcome);
            let tick = derive_tick(&guard, last_finished);
            events.publish(Event::QueueTick(tick));
        }
    }
}

/// Build a [`QueueTick`] from the on-disk queue document.
///
/// The caller hands ownership of an optional [`JobOutcomeSummary`]
/// captured at the same `save_atomic` boundary so a tick that closes
/// out a job can carry the just-recorded outcome without a second
/// pass over the jobs vector.
pub fn derive_tick(state: &QueueState, last_finished: Option<JobOutcomeSummary>) -> QueueTick {
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

fn summarize_outcome(job: &QueueJob) -> JobOutcomeSummary {
    JobOutcomeSummary {
        job_id: job.id.clone(),
        kind: job.kind,
        state: job.state,
        error: job.error.clone(),
        finished_at: job.finished_at.unwrap_or_else(Utc::now),
    }
}

/// Emit one terminating INFO event per job — a job-completion marker
/// that lands on its own line after the long tail of progress output
/// an ingest produces. On the daemon's stderr it gives the operator a
/// clean visual cue that the worker has finished; because it goes
/// through `tracing`, the same line also reaches the rolling log file
/// and the log broadcast channel, so remote observers (`logs --follow`,
/// the `log` event channel) see job outcomes directly instead of
/// inferring them from `queue.tick` summaries.
fn announce_outcome(job: &QueueJob, outcome: &JobOutcome) {
    let short: String = job.id.chars().take(8).collect();
    let name = job
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| job.path.display().to_string());
    match outcome {
        JobOutcome::Done => tracing::info!(job = %job.id, "queue: {short} {name} done"),
        JobOutcome::SkippedDuplicate { intake_id } => {
            tracing::info!(
                job = %job.id,
                "queue: {short} {name} skipped (already in catalog as intake {intake_id})"
            )
        }
        JobOutcome::NeedsOcr { intake_id } => {
            tracing::info!(
                job = %job.id,
                "queue: {short} {name} needs OCR (anchor intake {intake_id})"
            )
        }
        JobOutcome::Failed(msg) => {
            tracing::info!(job = %job.id, "queue: {short} {name} failed: {msg}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use std::path::PathBuf;

    fn sample_job() -> QueueJob {
        QueueJob {
            id: "01900000-0000-7000-8000-000000000001".to_string(),
            library: "default".to_string(),
            path: PathBuf::from("/tmp/example.epub"),
            kind: ItemKind::Book,
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            intake_ocr: None,
            audit_profile: None,
            state: JobState::Pending,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
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
            kind: ItemKind::Book,
            priority,
            force: false,
            hold_for_metadata: false,
            intake_ocr: None,
            audit_profile: None,
            state,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
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
        let ids = enqueue_files(
            &mut state,
            &paths,
            "books",
            ItemKind::Book,
            Priority::High,
            true,
            false,
            None,
        );
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

    #[test]
    fn apply_outcome_needs_ocr_sets_state_and_anchor_without_error() {
        let mut state = QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![job("xx", Priority::Normal, JobState::Running)],
        };
        state.jobs[0].error = Some("stale".to_string());
        apply_outcome(&mut state, "xx", JobOutcome::NeedsOcr { intake_id: 7 });
        assert_eq!(state.jobs[0].state, JobState::NeedsOcr);
        assert_eq!(state.jobs[0].merged_into, Some(7));
        assert!(state.jobs[0].error.is_none());
        assert!(state.jobs[0].finished_at.is_some());
    }

    #[tokio::test]
    async fn worker_loop_exits_on_shutdown_signal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let state = Arc::new(Mutex::new(QueueState::default()));
        let (tx, rx) = broadcast::channel::<()>(2);
        let handle = tokio::spawn(worker_loop(
            path,
            Arc::clone(&state),
            rx,
            |_| async { Ok::<JobSuccess, JobError>(JobSuccess::done()) },
            EventStreamHandle::default(),
            Arc::new(AtomicBool::new(false)),
        ));
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
            ItemKind::Book,
            Priority::Normal,
            false,
            false,
            None,
        );
        let state = Arc::new(Mutex::new(initial));
        let (tx, rx) = broadcast::channel::<()>(2);
        let handle = {
            let state = Arc::clone(&state);
            tokio::spawn(worker_loop(
                path.clone(),
                state,
                rx,
                |_job| async { Ok::<JobSuccess, JobError>(JobSuccess::done()) },
                EventStreamHandle::default(),
                Arc::new(AtomicBool::new(false)),
            ))
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

    #[tokio::test]
    async fn worker_loop_reads_working_for_the_execution_span() {
        use crate::control::events::{DaemonState, DaemonStateFlag};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut initial = QueueState::default();
        let _ids = enqueue_files(
            &mut initial,
            &[PathBuf::from("/tmp/only.epub")],
            "default",
            ItemKind::Book,
            Priority::Normal,
            false,
            false,
            None,
        );
        let state = Arc::new(Mutex::new(initial));
        let flag = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let events = EventStreamHandle::new(8, Arc::clone(&flag));
        let release = Arc::new(tokio::sync::Notify::new());
        let release_for_runner = Arc::clone(&release);
        let (tx, rx) = broadcast::channel::<()>(2);
        let handle = {
            let state = Arc::clone(&state);
            tokio::spawn(worker_loop(
                path,
                state,
                rx,
                move |_job| {
                    let release = Arc::clone(&release_for_runner);
                    async move {
                        release.notified().await;
                        Ok::<JobSuccess, JobError>(JobSuccess::done())
                    }
                },
                events,
                Arc::new(AtomicBool::new(false)),
            ))
        };

        // While the runner is blocked, the daemon must read `working`.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if flag.load() == DaemonState::Working {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "daemon never read `working` while the job ran"
            );
        }

        // Releasing the runner finishes the job and falls back to idle.
        release.notify_one();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let done = {
                let guard = state.lock().unwrap();
                guard.jobs.iter().any(|j| j.state == JobState::Done)
            };
            if done && flag.load() == DaemonState::Idle {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job did not finish and fall back to idle within 3 s"
            );
        }
        tx.send(()).expect("send shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn worker_loop_maps_needs_ocr_success_to_needs_ocr_state() {
        // The core risk is the worker-loop match order: a
        // `needs_ocr` success must be recognized ahead of the `no_op`
        // arms and land as `NeedsOcr` with the anchor id, not `Done`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut initial = QueueState::default();
        let _ids = enqueue_files(
            &mut initial,
            &[PathBuf::from("/tmp/scan.pdf")],
            "default",
            ItemKind::Book,
            Priority::Normal,
            false,
            false,
            None,
        );
        let state = Arc::new(Mutex::new(initial));
        let (tx, rx) = broadcast::channel::<()>(2);
        let handle = {
            let state = Arc::clone(&state);
            tokio::spawn(worker_loop(
                path,
                state,
                rx,
                |_job| async {
                    Ok::<JobSuccess, JobError>(JobSuccess {
                        no_op: false,
                        needs_ocr: true,
                        intake_id: Some(42),
                    })
                },
                EventStreamHandle::default(),
                Arc::new(AtomicBool::new(false)),
            ))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let settled = {
                let guard = state.lock().unwrap();
                guard.jobs.iter().any(|j| j.finished_at.is_some())
            };
            if settled {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not finish the job within 3 s"
            );
        }
        let snapshot = state.lock().unwrap().clone();
        assert_eq!(snapshot.jobs[0].state, JobState::NeedsOcr);
        assert_eq!(snapshot.jobs[0].merged_into, Some(42));
        assert!(snapshot.jobs[0].error.is_none());
        tx.send(()).expect("send shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn process_level_failure_pauses_queue_and_keeps_pending_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut initial = QueueState::default();
        let _ids = enqueue_files(
            &mut initial,
            &[PathBuf::from("/tmp/a.epub"), PathBuf::from("/tmp/b.epub")],
            "default",
            ItemKind::Book,
            Priority::Normal,
            false,
            false,
            None,
        );
        let state = Arc::new(Mutex::new(initial));
        let paused = Arc::new(AtomicBool::new(false));
        let (tx, rx) = broadcast::channel::<()>(2);
        let handle = {
            let state = Arc::clone(&state);
            let paused = Arc::clone(&paused);
            tokio::spawn(worker_loop(
                path.clone(),
                state,
                rx,
                |_job| async {
                    Err(JobError::Process(
                        "vector store error: Too many open files".to_string(),
                    ))
                },
                EventStreamHandle::default(),
                paused,
            ))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let failed = {
                let guard = state.lock().unwrap();
                guard.jobs.iter().any(|j| j.state == JobState::Failed)
            };
            if failed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not mark the job Failed within 3 s"
            );
        }
        // Give the worker a few more ticks: were the queue not paused,
        // it would pull and fail the second job too.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let snapshot = state.lock().unwrap().clone();
        assert!(paused.load(Ordering::Acquire), "pause flag not set");
        assert!(snapshot.paused, "persisted paused flag not set");
        let failed = snapshot
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Failed)
            .count();
        let pending = snapshot
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Pending)
            .count();
        assert_eq!(failed, 1, "exactly one job burns: {snapshot:?}");
        assert_eq!(pending, 1, "the second job survives: {snapshot:?}");
        tx.send(()).expect("send shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn process_level_failure_keeps_atomic_consistent_with_disk_when_persist_fails() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file in the parent dir forces save_atomic to fail:
        // `create_dir_all` cannot turn a file into a directory, so every
        // attempt against `<blocker>/queue.json` errors out cross-
        // platform.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"").unwrap();
        let path = blocker.join("queue.json");

        let mut initial = QueueState::default();
        let _ids = enqueue_files(
            &mut initial,
            &[PathBuf::from("/tmp/a.epub")],
            "default",
            ItemKind::Book,
            Priority::Normal,
            false,
            false,
            None,
        );
        let state = Arc::new(Mutex::new(initial));
        let paused = Arc::new(AtomicBool::new(false));
        let (tx, rx) = broadcast::channel::<()>(2);
        let handle = {
            let state = Arc::clone(&state);
            let paused = Arc::clone(&paused);
            tokio::spawn(worker_loop(
                path.clone(),
                state,
                rx,
                |_job| async {
                    Err(JobError::Process(
                        "vector store error: Too many open files".to_string(),
                    ))
                },
                EventStreamHandle::default(),
                paused,
            ))
        };

        // Give the worker time to pull, fail, and attempt to persist.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let snapshot = state.lock().unwrap().clone();
        let atomic_paused = paused.load(Ordering::Acquire);
        assert!(
            !atomic_paused,
            "atomic must not flip when save_atomic cannot persist"
        );
        assert!(
            !snapshot.paused,
            "in-memory QueueState::paused must be restored when save fails"
        );
        tx.send(()).expect("send shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    // Raw errno 24 is EMFILE on the unix platforms this test runs on;
    // other platforms assign the code differently.
    #[cfg(unix)]
    #[test]
    fn classify_marks_fd_exhaustion_errno_as_process_level() {
        let io = std::io::Error::from_raw_os_error(24);
        let err = eyre::Report::new(io).wrap_err("open vector store");
        assert!(matches!(classify_ingest_error(&err), JobError::Process(_)));
    }

    #[test]
    fn classify_marks_fd_exhaustion_message_as_process_level() {
        let err = eyre::eyre!("lance: IO error: Too many open files");
        assert!(matches!(classify_ingest_error(&err), JobError::Process(_)));
    }

    #[test]
    fn classify_marks_ordinary_failures_as_book_level() {
        let err = eyre::eyre!("source needs OCR and cannot be ingested as text");
        let classified = classify_ingest_error(&err);
        assert!(matches!(classified, JobError::Book(_)));
        let JobError::Book(message) = classified else {
            unreachable!()
        };
        assert!(message.starts_with("ingest: "), "got: {message}");
    }

    #[test]
    fn ingest_needs_ocr_recovers_the_anchor_from_a_wrapped_error() {
        // The typed error, wrapped exactly as the ops layer wraps it
        // before the worker sees it, still yields the anchor id.
        let typed = bookrack_ingest::IngestError::NeedsOcr {
            reason: "no text layer".to_string(),
            intake_id: 99,
        };
        let wrapped = eyre::Report::new(typed).wrap_err("registry-mediated ingest");
        assert_eq!(ingest_needs_ocr(&wrapped), Some(99));
    }

    #[test]
    fn ingest_needs_ocr_ignores_unrelated_and_string_errors() {
        // A plain string that merely mentions OCR is not the typed
        // variant, so it is not recognized — it flows through the
        // failure path instead.
        let strayed = eyre::eyre!("source needs OCR and cannot be ingested as text");
        assert_eq!(ingest_needs_ocr(&strayed), None);
    }
}
