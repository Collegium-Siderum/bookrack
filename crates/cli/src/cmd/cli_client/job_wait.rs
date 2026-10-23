// SPDX-License-Identifier: Apache-2.0

//! One wait path for every command that waits on queue jobs: the
//! `--wait` of `ingest`, `papers ingest` and `intake ocr`, and
//! `queue wait`.
//!
//! The caller subscribes to the event stream first, then the queue is
//! read once. That read fixes what the wait covers: a selected row that
//! has already ended becomes a record, the rest are waited on through
//! the stream. The worker and the pause flag are judged from the same
//! read, so a wait nothing will finish is refused before it starts. A
//! refusal still reports what the read showed: the ended rows as
//! records, the live ones by id.

use std::time::{Duration, Instant};

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::job_report::{JobOutcomeRecord, JobOutcomeReport, JobOutcomeState};
use bookrack_cli::render::progress::Surface;
use bookrack_control_client::{ControlClient, Event};
use bookrack_core::Problem;
use bookrack_core::queue::{JobState, QueueJob};
use eyre::{Context, Result, eyre};
use serde_json::Value;
use tokio::sync::broadcast;

use super::helpers;
use super::progress_view::ProgressView;

/// The daemon calls a wait makes.
pub(super) trait WaitSource {
    async fn subscribe(&self) -> Result<broadcast::Receiver<Event>>;
    async fn list(&self) -> Result<Value>;
    async fn worker_enabled(&self) -> Result<bool>;
}

/// [`WaitSource`] over a live control-plane connection.
pub(super) struct Daemon<'a>(pub &'a ControlClient);

impl WaitSource for Daemon<'_> {
    async fn subscribe(&self) -> Result<broadcast::Receiver<Event>> {
        self.0
            .subscribe()
            .await
            .context("subscribe to control-plane events")
    }

    async fn list(&self) -> Result<Value> {
        helpers::dispatch(self.0, "queue.list", Value::Null).await
    }

    async fn worker_enabled(&self) -> Result<bool> {
        helpers::queue_worker_enabled(self.0).await
    }
}

/// A `queue.list` reply: the typed rows, the same rows as sent, and the
/// pause flag.
pub(super) struct Listing {
    pub jobs: Vec<QueueJob>,
    pub rows: Vec<Value>,
    pub paused: bool,
}

impl Listing {
    pub fn parse(reply: &Value) -> Result<Self> {
        let rows = reply
            .get("jobs")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| eyre!("queue.list reply carries no jobs array"))?;
        let jobs = rows
            .iter()
            .map(|row| serde_json::from_value::<QueueJob>(row.clone()))
            .collect::<Result<Vec<_>, _>>()
            .context("parse queue.list rows")?;
        let paused = reply
            .get("paused")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(Self { jobs, rows, paused })
    }
}

pub(super) fn is_live(state: JobState) -> bool {
    matches!(state, JobState::Pending | JobState::Running)
}

/// What a wait covers once the queue has been read: the selected rows
/// that had ended, as records, and the ids still pending or running,
/// in selection order.
pub(super) struct Attached {
    pub settled: Vec<JobOutcomeRecord>,
    pub live: Vec<String>,
}

/// Indices of the rows whose id is in `ids`, in the order given, an id
/// named twice counting once. The ids came from the daemon, so an id
/// without a row is a data error rather than a refusal.
pub(super) fn rows_for_ids(listing: &Listing, ids: &[String]) -> Result<Vec<usize>> {
    let mut picked = Vec::new();
    for id in ids {
        let index = listing
            .jobs
            .iter()
            .position(|job| &job.id == id)
            .ok_or_else(|| eyre!("queue.list carries no row for job {id}"))?;
        if !picked.contains(&index) {
            picked.push(index);
        }
    }
    Ok(picked)
}

/// Splits the selected rows into ended records and live ids.
pub(super) fn attach(listing: &Listing, indices: &[usize]) -> Result<Attached> {
    let mut settled = Vec::new();
    let mut live = Vec::new();
    for &index in indices {
        let job = &listing.jobs[index];
        if is_live(job.state) {
            live.push(job.id.clone());
        } else {
            settled.push(ended_record(job, &listing.rows[index])?);
        }
    }
    Ok(Attached { settled, live })
}

/// The judgement made when a wait attaches, from the queue as read:
/// live jobs on a daemon without a queue worker, or pending ones on a
/// paused queue, are refused. `None` means the wait may proceed. The
/// worker is asked about only when a live job needs it.
pub(super) async fn judge<S: WaitSource>(
    source: &S,
    listing: &Listing,
    attached: &Attached,
) -> Result<Option<Problem>> {
    if attached.live.is_empty() {
        return Ok(None);
    }
    if !source.worker_enabled().await? {
        return Ok(Some(no_worker(attached.live.len())));
    }
    let pending = pending_among(listing, &attached.live);
    if listing.paused && pending > 0 {
        return Ok(Some(paused(pending)));
    }
    Ok(None)
}

/// How many of `ids` the listing reports as pending.
fn pending_among(listing: &Listing, ids: &[String]) -> usize {
    listing
        .jobs
        .iter()
        .filter(|job| job.state == JobState::Pending && ids.contains(&job.id))
        .count()
}

/// How a wait ended.
#[derive(Debug)]
pub(super) enum WaitEnd {
    /// Every covered job ended.
    Complete(JobOutcomeReport),
    /// The wait stopped before every job ended. `report` carries the
    /// records collected so far and names the rest in `unfinished`.
    Refused {
        report: JobOutcomeReport,
        problem: Problem,
    },
}

/// Judge, then wait on the live ids. `rx` was subscribed before the
/// rows were read, so a job that ends after the read reaches it.
/// `stall` bounds the stretch between two events.
pub(super) async fn wait<S: WaitSource>(
    source: &S,
    rx: broadcast::Receiver<Event>,
    listing: &Listing,
    attached: Attached,
    surface: Box<dyn Surface>,
    started: Instant,
    stall: Duration,
) -> Result<WaitEnd> {
    if let Some(problem) = judge(source, listing, &attached).await? {
        let report = JobOutcomeReport::new(attached.settled, started.elapsed())
            .with_unfinished(attached.live);
        return Ok(WaitEnd::Refused { report, problem });
    }
    let view = ProgressView::for_jobs_after(&attached.settled, &attached.live, surface);
    let awaited = await_jobs_from_rx(rx, view, started, stall).await?;
    let mut jobs = attached.settled;
    jobs.extend(awaited.jobs);
    Ok(WaitEnd::Complete(JobOutcomeReport::new(
        jobs,
        started.elapsed(),
    )))
}

/// Read the queue, attach to `ids`, judge, wait: the path a submission
/// takes once the daemon has answered with the ids it queued.
pub(super) async fn wait_on_ids<S: WaitSource>(
    source: &S,
    rx: broadcast::Receiver<Event>,
    ids: &[String],
    surface: Box<dyn Surface>,
    stall: Duration,
) -> Result<WaitEnd> {
    let started = Instant::now();
    let listing = Listing::parse(&source.list().await?)?;
    let indices = rows_for_ids(&listing, ids)?;
    let attached = attach(&listing, &indices)?;
    wait(source, rx, &listing, attached, surface, started, stall).await
}

/// Print the report and turn the end into the exit code. `Complete`
/// is [`helpers::finalize_job_batch`]; `Refused` prints the partial
/// report the same way, then returns the refusal, so the problem
/// reaches stderr after the report has reached stdout.
pub(super) fn finalize(end: WaitEnd, scope: &'static str, action: &str, label: &str) -> Result<()> {
    match end {
        WaitEnd::Complete(report) => helpers::finalize_job_batch(&report, scope, action, label),
        WaitEnd::Refused { report, problem } => {
            helpers::emit_job_summary(&report, action, label);
            Err(refused(problem).into())
        }
    }
}

pub(super) fn refused(problem: Problem) -> BookrackCliError {
    BookrackCliError::QueueWaitRefused { problem }
}

/// The outcome record an ended row reads as. `kind` and `finished_at`
/// are copied from the row as the daemon sent them, the same spelling
/// its `last_finished` event carries.
fn ended_record(job: &QueueJob, row: &Value) -> Result<JobOutcomeRecord> {
    let wire_state = job.state.as_wire_str();
    let state = JobOutcomeState::from_wire(wire_state)
        .ok_or_else(|| eyre!("job {} is {wire_state}, not an ended state", job.id))?;
    let finished_at = row
        .get("finished_at")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            eyre!(
                "queue.list reported job {} as {wire_state} without a finished_at",
                job.id
            )
        })?;
    let kind = row
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("queue.list reported job {} without a kind", job.id))?;
    Ok(JobOutcomeRecord {
        job_id: job.id.clone(),
        kind: kind.to_string(),
        state,
        error: job.error.clone(),
        finished_at: finished_at.to_string(),
    })
}

const RESUME_HINT: &str =
    "Run `bookrack queue resume`, then `bookrack queue wait` to pick up what is still queued.";

fn no_worker(live: usize) -> Problem {
    let detail = if live == 1 {
        "1 selected job is pending or running, and nothing will run it.".to_string()
    } else {
        format!("{live} selected jobs are pending or running, and nothing will run them.")
    };
    Problem::new("cannot wait on queue jobs: the daemon has no queue worker")
        .detail(detail)
        .hint("Restart the daemon with a queue worker: `bookrack run`.")
}

fn paused(pending: usize) -> Problem {
    let detail = if pending == 1 {
        "1 selected job is pending; a paused worker pulls no job.".to_string()
    } else {
        format!("{pending} selected jobs are pending; a paused worker pulls no job.")
    };
    Problem::new("cannot wait on pending jobs while the queue is paused")
        .detail(detail)
        .hint(RESUME_HINT)
}

/// Drains the receiver until every job `view` awaits has appeared in a
/// `queue.tick`'s `last_finished`.
///
/// `stall_timeout` bounds the wait between consecutive events. The
/// timer resets every time an event lands; a stretch with no events
/// at all surfaces as an error instead of hanging the CLI.
pub(super) async fn await_jobs_from_rx(
    mut rx: broadcast::Receiver<Event>,
    mut view: ProgressView,
    started_at: Instant,
    stall_timeout: Duration,
) -> Result<JobOutcomeReport> {
    let mut jobs: Vec<JobOutcomeRecord> = Vec::with_capacity(view.pending());
    while !view.is_complete() {
        match tokio::time::timeout(stall_timeout, rx.recv()).await {
            Err(_elapsed) => {
                return Err(eyre::eyre!(
                    "control event stream stalled for {}s with {} job(s) still pending; \
                     daemon may be unresponsive",
                    stall_timeout.as_secs(),
                    view.pending()
                ));
            }
            Ok(Ok(event)) => {
                if let Some(record) = view.apply(&event) {
                    jobs.push(record);
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err(eyre::eyre!(
                    "control event stream closed before {} job(s) finished",
                    view.pending()
                ));
            }
        }
    }
    Ok(JobOutcomeReport::new(jobs, started_at.elapsed()))
}

/// A fake daemon and the rows and events the wait tests feed it.
#[cfg(test)]
pub(super) mod testing {
    use std::sync::Mutex;

    use bookrack_cli::render::progress::{Surface, SurfaceKind};
    use bookrack_control_client::Event;
    use eyre::Result;
    use serde_json::{Value, json};
    use tokio::sync::broadcast;

    use super::WaitSource;

    // Ids as one minute's submissions generate them: a shared timestamp
    // head, distinct random tails.
    pub const A: &str = "019a1b2c-3d4e-7f00-8a11-0000aaaaaaaa";
    pub const B: &str = "019a1b2c-3d4e-7f00-8a11-0000bbbbbbbb";
    pub const D: &str = "019a1b2c-3d4e-7f00-8a11-0000dddddddd";
    pub const N: &str = "019a1b2c-3d4e-7f00-8a11-0000eeeeeeee";
    pub const P: &str = "019a1b2c-3d4e-7f00-8a11-000011111111";
    pub const R: &str = "019a1b2c-3d4e-7f00-8a11-000022222222";
    pub const R1: &str = "019a1b2c-3d4e-7f00-8a11-000033333333";
    pub const R2: &str = "019a1b2c-3d4e-7f00-8a11-000044444444";

    pub const FINISHED_AT: &str = "2026-01-02T03:05:05.123456789Z";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Call {
        Subscribe,
        List,
        WorkerEnabled,
    }

    /// A daemon whose `queue.list` reply and worker flag are fixed. The
    /// events in `on_list` are broadcast inside `list`, before the reply
    /// returns, so they reach only a subscription made earlier.
    pub struct FakeSource {
        pub tx: broadcast::Sender<Event>,
        calls: Mutex<Vec<Call>>,
        reply: Value,
        worker: bool,
        on_list: Vec<Event>,
    }

    impl FakeSource {
        pub fn new(rows: Vec<Value>, paused: bool, on_list: Vec<Event>) -> Self {
            let (tx, _) = broadcast::channel(64);
            Self {
                tx,
                calls: Mutex::new(Vec::new()),
                reply: json!({ "paused": paused, "jobs": rows }),
                worker: true,
                on_list,
            }
        }

        pub fn without_worker(mut self) -> Self {
            self.worker = false;
            self
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl WaitSource for FakeSource {
        async fn subscribe(&self) -> Result<broadcast::Receiver<Event>> {
            self.calls.lock().unwrap().push(Call::Subscribe);
            Ok(self.tx.subscribe())
        }

        async fn list(&self) -> Result<Value> {
            self.calls.lock().unwrap().push(Call::List);
            for event in &self.on_list {
                let _ = self.tx.send(event.clone());
            }
            Ok(self.reply.clone())
        }

        async fn worker_enabled(&self) -> Result<bool> {
            self.calls.lock().unwrap().push(Call::WorkerEnabled);
            Ok(self.worker)
        }
    }

    pub fn row(id: &str, state: &str) -> Value {
        row_with(id, state, "book", None)
    }

    pub fn row_with(id: &str, state: &str, kind: &str, error: Option<&str>) -> Value {
        let ended = !matches!(state, "pending" | "running");
        json!({
            "id": id,
            "library": "main",
            "path": format!("/books/{id}.epub"),
            "priority": "normal",
            "force": false,
            "hold_for_metadata": false,
            "kind": kind,
            "intake_ocr": null,
            "audit_profile": null,
            "state": state,
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": if state == "pending" { Value::Null } else { json!("2026-01-02T03:04:06Z") },
            "finished_at": if ended { json!(FINISHED_AT) } else { Value::Null },
            "error": error,
            "merged_into": null,
        })
    }

    /// A `queue.tick` whose `last_finished` ends `id`.
    pub fn finished(id: &str, state: &str, kind: &str, error: Option<&str>) -> Event {
        let mut last = json!({
            "job_id": id,
            "kind": kind,
            "state": state,
            "finished_at": FINISHED_AT,
        });
        if let Some(error) = error {
            last["error"] = json!(error);
        }
        Event {
            channel: "queue.tick".to_string(),
            value: json!({ "pending": 0, "running": 0, "last_finished": last }),
            lag: false,
        }
    }

    /// A `queue.tick` that ends nothing.
    pub fn tick_without_finished() -> Event {
        Event {
            channel: "queue.tick".to_string(),
            value: json!({ "pending": 1, "running": 0 }),
            lag: false,
        }
    }

    pub fn hidden() -> Box<dyn Surface> {
        SurfaceKind::Hidden.build()
    }

    pub fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use bookrack_cli::error::classify_eyre;
    use bookrack_cli::render::progress::SurfaceKind;
    use serde_json::json;

    use super::super::progress_view::testing::RecordingSurface;
    use super::super::progress_view::{ViewState, extract_finished};
    use super::testing::*;
    use super::*;

    const STALL: Duration = Duration::from_secs(2);

    /// Loose default for in-test waits: long enough that the event
    /// loop can't race the timer in CI, short enough that the stall
    /// test still finishes promptly.
    const TEST_STALL_TIMEOUT: Duration = Duration::from_secs(5);

    /// A view over `ids` that draws nothing.
    fn quiet_view(list: &[&str]) -> ProgressView {
        ProgressView::with_surface(ViewState::for_jobs(&ids(list)), SurfaceKind::Hidden.build())
    }

    /// A view over `ids` whose surface calls land in the returned log.
    fn recorded_view(list: &[&str]) -> (ProgressView, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let view = ProgressView::with_surface(
            ViewState::for_jobs(&ids(list)),
            Box::new(RecordingSurface(Arc::clone(&log))),
        );
        (view, log)
    }

    fn tick(job_id: &str, state: &str, pending: u64, running: u64) -> Event {
        Event {
            channel: "queue.tick".to_string(),
            value: json!({
                "pending": pending,
                "running": running,
                "last_finished": {
                    "job_id": job_id,
                    "kind": "book",
                    "state": state,
                    "finished_at": "2026-01-01T00:00:00Z",
                },
            }),
            lag: false,
        }
    }

    fn refusal(end: &WaitEnd) -> (&JobOutcomeReport, &Problem) {
        match end {
            WaitEnd::Refused { report, problem } => (report, problem),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn await_jobs_returns_immediately_when_empty() {
        let (_tx, rx) = broadcast::channel::<Event>(4);
        let report = await_jobs_from_rx(rx, quiet_view(&[]), Instant::now(), TEST_STALL_TIMEOUT)
            .await
            .unwrap();
        assert!(report.jobs.is_empty());
        assert_eq!(report.totals.done, 0);
    }

    #[tokio::test]
    async fn await_jobs_collects_all_three_terminal_states() {
        let (tx, rx) = broadcast::channel::<Event>(16);
        let view = quiet_view(&["a", "b", "c"]);
        let handle = tokio::spawn(async move {
            await_jobs_from_rx(rx, view, Instant::now(), TEST_STALL_TIMEOUT).await
        });
        tx.send(tick("a", "done", 2, 1)).unwrap();
        tx.send(tick("b", "failed", 1, 1)).unwrap();
        tx.send(tick("c", "cancelled", 0, 0)).unwrap();
        let report = handle.await.unwrap().unwrap();
        assert_eq!(report.jobs.len(), 3);
        assert_eq!(report.totals.done, 1);
        assert_eq!(report.totals.failed, 1);
        assert_eq!(report.totals.cancelled, 1);
        assert!(!report.all_succeeded());
    }

    #[tokio::test]
    async fn await_jobs_ignores_ticks_for_other_ids_and_empty_finished() {
        let (tx, rx) = broadcast::channel::<Event>(16);
        let view = quiet_view(&["target"]);
        let handle = tokio::spawn(async move {
            await_jobs_from_rx(rx, view, Instant::now(), TEST_STALL_TIMEOUT).await
        });
        tx.send(tick_without_finished()).unwrap();
        tx.send(tick("other", "done", 1, 0)).unwrap();
        tx.send(tick("target", "done", 0, 0)).unwrap();
        let report = handle.await.unwrap().unwrap();
        assert_eq!(report.jobs.len(), 1);
        assert_eq!(report.jobs[0].job_id, "target");
        assert!(report.all_succeeded());
    }

    /// A daemon that accepts the job but then stops emitting any
    /// events at all must not leave the CLI hanging: the stall timer
    /// elapses and the wait returns an actionable error.
    #[tokio::test]
    async fn await_jobs_errors_when_stream_stalls_with_no_events() {
        let (tx, rx) = broadcast::channel::<Event>(4);
        let (view, log) = recorded_view(&["a"]);
        let started = Instant::now();
        let err = await_jobs_from_rx(rx, view, started, Duration::from_millis(50))
            .await
            .unwrap_err();
        drop(tx);
        let msg = err.to_string();
        assert!(msg.contains("stalled"), "error text: {msg}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stall fired after {:?}",
            started.elapsed()
        );
        assert_eq!(
            log.lock().unwrap().last().map(String::as_str),
            Some("clear"),
            "a stalled wait left its progress line drawn"
        );
    }

    #[tokio::test]
    async fn await_jobs_errors_when_stream_closes_with_pending() {
        let (tx, rx) = broadcast::channel::<Event>(4);
        let (view, log) = recorded_view(&["a"]);
        let handle = tokio::spawn(async move {
            await_jobs_from_rx(rx, view, Instant::now(), TEST_STALL_TIMEOUT).await
        });
        drop(tx);
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("1 job"));
        assert_eq!(
            log.lock().unwrap().last().map(String::as_str),
            Some("clear"),
            "a closed stream left the progress line drawn"
        );
    }

    #[tokio::test]
    async fn a_submission_waited_on_a_paused_queue_is_refused_with_its_ids() {
        let source = FakeSource::new(
            vec![
                row_with(A, "failed", "book", Some("boom")),
                row(P, "pending"),
                row(R, "pending"),
            ],
            true,
            Vec::new(),
        );
        let rx = source.subscribe().await.unwrap();
        let end = wait_on_ids(&source, rx, &ids(&[A, P, R]), hidden(), STALL)
            .await
            .unwrap();
        let (report, problem) = refusal(&end);
        assert_eq!(report.jobs.len(), 1);
        assert_eq!(report.jobs[0].job_id, A);
        assert_eq!(report.jobs[0].error.as_deref(), Some("boom"));
        assert_eq!(report.unfinished, ids(&[P, R]));
        assert!(!report.all_succeeded());
        assert_eq!(
            problem.summary,
            "cannot wait on pending jobs while the queue is paused"
        );
        assert!(
            problem
                .data
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("bookrack queue wait")),
            "{problem:?}"
        );
        assert_eq!(
            source.calls(),
            [Call::Subscribe, Call::List, Call::WorkerEnabled]
        );

        let err = finalize(end, "ingest", "Ingested", "books").unwrap_err();
        assert_eq!(classify_eyre(&err).unwrap().as_cli().exit_code(), 2);
    }

    #[tokio::test]
    async fn a_submission_waited_without_a_worker_is_refused_with_its_ids() {
        let source = FakeSource::new(vec![row(P, "pending"), row(D, "done")], false, Vec::new())
            .without_worker();
        let rx = source.subscribe().await.unwrap();
        let end = wait_on_ids(&source, rx, &ids(&[P, D]), hidden(), STALL)
            .await
            .unwrap();
        let (report, problem) = refusal(&end);
        assert_eq!(report.jobs.len(), 1);
        assert_eq!(report.jobs[0].job_id, D);
        assert_eq!(report.unfinished, ids(&[P]));
        assert_eq!(
            problem.summary,
            "cannot wait on queue jobs: the daemon has no queue worker"
        );
    }

    #[tokio::test]
    async fn a_submission_waits_on_its_live_ids_and_keeps_the_ended_ones() {
        let source = FakeSource::new(
            vec![row(D, "done"), row(R, "running"), row(N, "pending")],
            false,
            vec![finished(R, "done", "book", None)],
        );
        let rx = source.subscribe().await.unwrap();
        let end = wait_on_ids(&source, rx, &ids(&[D, R, D]), hidden(), STALL)
            .await
            .unwrap();
        let WaitEnd::Complete(report) = end else {
            panic!("expected a complete report, got {end:?}");
        };
        let seen: Vec<&str> = report.jobs.iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(seen, [D, R], "the row named twice counts once");
        assert!(report.is_complete());
        assert!(report.all_succeeded());
    }

    #[tokio::test]
    async fn a_submitted_id_without_a_row_is_a_data_error() {
        let source = FakeSource::new(vec![row(P, "pending")], false, Vec::new());
        let rx = source.subscribe().await.unwrap();
        let err = wait_on_ids(&source, rx, &ids(&[P, A]), hidden(), STALL)
            .await
            .unwrap_err();
        assert!(classify_eyre(&err).is_none(), "{err:?}");
        assert!(err.to_string().contains(A), "{err}");
    }

    /// A row that never reached the wait set would surface as a stall
    /// after `STALL`; the data error names the row and returns at once.
    #[tokio::test]
    async fn an_ended_row_without_finished_at_is_a_data_error() {
        let mut broken = row_with(A, "failed", "book", Some("boom"));
        broken["finished_at"] = Value::Null;
        let source = FakeSource::new(vec![broken], false, Vec::new());
        let rx = source.subscribe().await.unwrap();
        let err = wait_on_ids(&source, rx, &ids(&[A]), hidden(), STALL)
            .await
            .unwrap_err();
        assert!(classify_eyre(&err).is_none(), "{err:?}");
        let message = err.to_string();
        assert!(
            message.contains(A) && message.contains("finished_at"),
            "{message}"
        );
    }

    /// The same literal outcome, once as a listed row and once as the
    /// `last_finished` of a tick, must read as the same record.
    #[test]
    fn an_ended_row_converts_to_the_record_its_finish_event_would_give() {
        let cases = [
            ("done", true),
            ("skipped_duplicate", true),
            ("needs_ocr", true),
            ("failed", false),
            ("cancelled", false),
        ];
        for (state, succeeds) in cases {
            for kind in ["book", "paper"] {
                let error = (state == "failed").then_some("ingest: boom: no body blocks");
                let row = row_with(A, state, kind, error);
                let job: QueueJob = serde_json::from_value(row.clone()).unwrap();
                let from_row = ended_record(&job, &row).unwrap();

                let pending: HashSet<String> = HashSet::from([A.to_string()]);
                let from_event =
                    extract_finished(&finished(A, state, kind, error).value, &pending).unwrap();

                assert_eq!(
                    serde_json::to_value(&from_row).unwrap(),
                    serde_json::to_value(&from_event).unwrap(),
                    "{state} / {kind}"
                );
                assert_eq!(from_row.finished_at, FINISHED_AT, "{state} / {kind}");
                assert_eq!(from_row.kind, kind, "{state} / {kind}");
                assert_eq!(
                    JobOutcomeReport::new(vec![from_row], Duration::ZERO).all_succeeded(),
                    succeeds,
                    "{state} / {kind}"
                );
            }
        }
    }
}
