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
    await_jobs_reconciling(
        source,
        rx,
        view,
        attached.settled,
        attached.live,
        started,
        stall,
    )
    .await
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

/// Waits on the jobs `view` awaits, reconciling against the queue
/// whenever the stream goes quiet or drops events.
///
/// A job ends when a `queue.tick` carries its outcome. Only an event
/// about an awaited job — its progress, or its outcome — extends the
/// deadline; a stretch of `stall` without one, a lag marker from either
/// side of the stream, or the daemon reporting itself degraded all make
/// the wait read the queue: awaited rows that have ended there are
/// settled from the listing, and pending rows on a paused queue end the
/// wait with the same refusal the attach makes. Only a `queue.list` that
/// fails ends the wait as an error. The deadline is compared before
/// every turn on the receiver, so a receiver that is never empty cannot
/// starve a deadline that has passed; the timer runs with the
/// runtime's millisecond granularity, which is why the comparison is
/// not left to the timer alone.
pub(super) async fn await_jobs_reconciling<S: WaitSource>(
    source: &S,
    mut rx: broadcast::Receiver<Event>,
    mut view: ProgressView,
    settled: Vec<JobOutcomeRecord>,
    live: Vec<String>,
    started: Instant,
    stall: Duration,
) -> Result<WaitEnd> {
    let mut jobs = settled;
    let mut deadline = tokio::time::Instant::now() + stall;
    while !view.is_complete() {
        let reconcile = tokio::time::Instant::now() >= deadline
            || tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => true,
                received = rx.recv() => match received {
                    Ok(event) => {
                        let applied = view.apply(&event);
                        if let Some(record) = applied.finished {
                            jobs.push(record);
                        }
                        if applied.relevant {
                            deadline = tokio::time::Instant::now() + stall;
                        }
                        event.lag || is_degraded(&event)
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => true,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(eyre!(
                            "control event stream closed before {} job(s) finished",
                            view.pending()
                        ));
                    }
                },
            };
        if !reconcile {
            continue;
        }
        let awaited = view.pending();
        let reply = source.list().await.wrap_err_with(|| {
            format!("queue.list did not answer with {awaited} job(s) still awaited")
        })?;
        let listing = Listing::parse(&reply)?;
        for id in &live {
            if !view.awaits(id) {
                continue;
            }
            let index = listing
                .jobs
                .iter()
                .position(|job| &job.id == id)
                .ok_or_else(|| eyre!("queue.list carries no row for job {id}"))?;
            let job = &listing.jobs[index];
            if is_live(job.state) {
                continue;
            }
            if let Some(record) = view.settle(ended_record(job, &listing.rows[index])?) {
                jobs.push(record);
            }
        }
        if view.is_complete() {
            break;
        }
        let unfinished: Vec<String> = live.iter().filter(|id| view.awaits(id)).cloned().collect();
        let pending = pending_among(&listing, &unfinished);
        if listing.paused && pending > 0 {
            return Ok(WaitEnd::Refused {
                report: JobOutcomeReport::new(jobs, started.elapsed()).with_unfinished(unfinished),
                problem: paused(pending),
            });
        }
        deadline = tokio::time::Instant::now() + stall;
    }
    Ok(WaitEnd::Complete(JobOutcomeReport::new(
        jobs,
        started.elapsed(),
    )))
}

/// A `daemon.state` event reporting the daemon degraded: the state a
/// queue worker folds in when it pauses the queue after a process-level
/// failure.
fn is_degraded(event: &Event) -> bool {
    event.channel == "daemon.state" && event.value.as_str() == Some("degraded")
}

/// A fake daemon and the rows and events the wait tests feed it.
#[cfg(test)]
pub(super) mod testing {
    use std::sync::{Arc, Mutex};

    use bookrack_cli::render::progress::{Surface, SurfaceKind};
    use bookrack_control_client::{ControlError, Event};
    use eyre::{Context, Result};
    use serde_json::{Value, json};
    use tokio::sync::{Notify, broadcast};

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
    /// events in `on_list` are broadcast
    /// inside `list`, before the reply returns, so they reach only a
    /// subscription made earlier. Every `list` is recorded in `calls`,
    /// in `log` when one is shared with a surface, and signalled on
    /// `listed` so a test can wait for the first reconciliation.
    pub struct FakeSource {
        pub tx: broadcast::Sender<Event>,
        pub listed: Notify,
        calls: Mutex<Vec<Call>>,
        reply: Value,
        fail_list: Mutex<Option<ControlError>>,
        worker: bool,
        on_list: Vec<Event>,
        log: Option<Arc<Mutex<Vec<String>>>>,
    }

    impl FakeSource {
        pub fn new(rows: Vec<Value>, paused: bool, on_list: Vec<Event>) -> Self {
            let (tx, _) = broadcast::channel(64);
            Self {
                tx,
                listed: Notify::new(),
                calls: Mutex::new(Vec::new()),
                reply: json!({ "paused": paused, "jobs": rows }),
                fail_list: Mutex::new(None),
                worker: true,
                on_list,
                log: None,
            }
        }

        pub fn without_worker(mut self) -> Self {
            self.worker = false;
            self
        }

        /// Records each `list` as the line `list` in `log`, beside what
        /// a surface sharing it draws.
        pub fn logging_to(mut self, log: Arc<Mutex<Vec<String>>>) -> Self {
            self.log = Some(log);
            self
        }

        /// The next `list` fails with `error` instead of answering.
        pub fn failing_list(self, error: ControlError) -> Self {
            *self.fail_list.lock().unwrap() = Some(error);
            self
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        pub fn lists(&self) -> usize {
            self.calls().iter().filter(|c| **c == Call::List).count()
        }
    }

    impl WaitSource for FakeSource {
        async fn subscribe(&self) -> Result<broadcast::Receiver<Event>> {
            self.calls.lock().unwrap().push(Call::Subscribe);
            Ok(self.tx.subscribe())
        }

        async fn list(&self) -> Result<Value> {
            self.calls.lock().unwrap().push(Call::List);
            if let Some(log) = &self.log {
                log.lock().unwrap().push("list".to_string());
            }
            self.listed.notify_one();
            if let Some(error) = self.fail_list.lock().unwrap().take() {
                return Err(eyre::Report::from(error)).context("queue.list rpc");
            }
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

    /// A `worker.progress` for `id` in `stage`.
    pub fn progress(id: &str, stage: &str) -> Event {
        Event {
            channel: "worker.progress".to_string(),
            value: json!({ "job_id": id, "kind": "book", "stage": stage }),
            lag: false,
        }
    }

    /// A `daemon.state` event with the given state word.
    pub fn daemon_state(state: &str) -> Event {
        Event {
            channel: "daemon.state".to_string(),
            value: json!(state),
            lag: false,
        }
    }

    /// The lag marker the daemon sends for one snapshot channel.
    pub fn lag_marker() -> Event {
        Event {
            channel: "queue.tick".to_string(),
            value: Value::Null,
            lag: true,
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
    use bookrack_control_client::ControlError;

    use super::super::progress_view::extract_finished;
    use super::super::progress_view::testing::RecordingSurface;
    use super::testing::*;
    use super::*;

    const STALL: Duration = Duration::from_millis(300);

    /// Bound on a test's own wait: a loop that never reconciles, or
    /// never completes, is red here instead of hanging.
    const LIMIT: Duration = Duration::from_millis(1200);

    /// A view over `live` that draws nothing.
    fn quiet_view(live: &[&str]) -> ProgressView {
        ProgressView::for_jobs_after(&[], &ids(live), SurfaceKind::Hidden.build())
    }

    /// A view over `live` whose surface calls land in `log`.
    fn logged_view(live: &[&str], log: &Arc<Mutex<Vec<String>>>) -> ProgressView {
        ProgressView::for_jobs_after(&[], &ids(live), Box::new(RecordingSurface(Arc::clone(log))))
    }

    /// Runs the wait core on `source`'s own stream over `live`, bounded
    /// by [`LIMIT`].
    async fn reconciling(
        source: &FakeSource,
        rx: broadcast::Receiver<Event>,
        view: ProgressView,
        live: &[&str],
    ) -> Result<WaitEnd> {
        tokio::time::timeout(
            LIMIT,
            await_jobs_reconciling(
                source,
                rx,
                view,
                Vec::new(),
                ids(live),
                Instant::now(),
                STALL,
            ),
        )
        .await
        .expect("the wait neither completed nor reconciled within the limit")
    }

    fn complete(end: WaitEnd) -> JobOutcomeReport {
        match end {
            WaitEnd::Complete(report) => report,
            other => panic!("expected a complete report, got {other:?}"),
        }
    }

    fn refusal(end: &WaitEnd) -> (&JobOutcomeReport, &Problem) {
        match end {
            WaitEnd::Refused { report, problem } => (report, problem),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn states(report: &JobOutcomeReport) -> Vec<(String, JobOutcomeState)> {
        report
            .jobs
            .iter()
            .map(|job| (job.job_id.clone(), job.state))
            .collect()
    }

    #[tokio::test]
    async fn an_empty_wait_set_completes_at_once() {
        let source = FakeSource::new(Vec::new(), false, Vec::new());
        let rx = source.tx.subscribe();
        let report = complete(
            reconciling(&source, rx, quiet_view(&[]), &[])
                .await
                .unwrap(),
        );
        assert!(report.jobs.is_empty());
        assert_eq!(source.lists(), 0);
    }

    #[tokio::test]
    async fn outcomes_on_the_stream_are_collected_in_arrival_order() {
        let source = FakeSource::new(Vec::new(), false, Vec::new());
        let rx = source.tx.subscribe();
        source.tx.send(finished(A, "done", "book", None)).unwrap();
        source.tx.send(tick_without_finished()).unwrap();
        source.tx.send(finished(N, "done", "book", None)).unwrap();
        source
            .tx
            .send(finished(B, "failed", "book", Some("boom")))
            .unwrap();
        source
            .tx
            .send(finished(D, "cancelled", "book", None))
            .unwrap();
        let report = complete(
            reconciling(&source, rx, quiet_view(&[A, B, D]), &[A, B, D])
                .await
                .unwrap(),
        );
        assert_eq!(
            states(&report),
            [
                (A.to_string(), JobOutcomeState::Done),
                (B.to_string(), JobOutcomeState::Failed),
                (D.to_string(), JobOutcomeState::Cancelled),
            ]
        );
        assert!(!report.all_succeeded());
        assert_eq!(source.lists(), 0, "a stream that delivers needs no listing");
    }

    #[tokio::test]
    async fn a_closed_stream_ends_the_wait_as_an_error() {
        let source = FakeSource::new(Vec::new(), false, Vec::new());
        let (tx, rx) = broadcast::channel::<Event>(4);
        drop(tx);
        let log = Arc::new(Mutex::new(Vec::new()));
        let err = reconciling(&source, rx, logged_view(&[A], &log), &[A])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1 job"), "{err}");
        assert_eq!(
            log.lock().unwrap().last().map(String::as_str),
            Some("clear"),
            "a closed stream left the progress line drawn"
        );
    }

    /// B.5 #1: a queue paused after the wait attached sends nothing; the
    /// quiet stretch ends in a listing, and the pause is a refusal.
    #[tokio::test]
    async fn a_paused_queue_found_on_a_quiet_stream_refuses_with_the_pending_ids() {
        let source = FakeSource::new(vec![row(P, "pending")], true, Vec::new());
        let rx = source.tx.subscribe();
        let log = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        let end = reconciling(&source, rx, logged_view(&[P], &log), &[P])
            .await
            .unwrap();
        assert!(
            started.elapsed() >= STALL,
            "reconciled early: {:?}",
            started.elapsed()
        );
        let (report, problem) = refusal(&end);
        assert!(report.jobs.is_empty());
        assert_eq!(report.unfinished, ids(&[P]));
        assert_eq!(
            problem.summary,
            "cannot wait on pending jobs while the queue is paused"
        );
        assert_eq!(
            log.lock().unwrap().last().map(String::as_str),
            Some("clear"),
            "a refused wait left its progress line drawn"
        );
    }

    /// B.5 #2: the target was cancelled without an outcome event while
    /// other jobs keep the stream busy; their events do not postpone the
    /// listing.
    #[tokio::test]
    async fn a_cancelled_target_is_reconciled_while_other_jobs_keep_ticking() {
        let source = Arc::new(FakeSource::new(
            vec![row(A, "cancelled")],
            false,
            Vec::new(),
        ));
        let rx = source.tx.subscribe();
        source.tx.send(tick_without_finished()).unwrap();
        let noise = {
            let source = Arc::clone(&source);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(STALL / 4).await;
                    let _ = source.tx.send(progress(N, "processing"));
                    let _ = source.tx.send(finished(N, "done", "book", None));
                }
            })
        };
        let started = Instant::now();
        let report = complete(
            reconciling(&source, rx, quiet_view(&[A]), &[A])
                .await
                .unwrap(),
        );
        noise.abort();
        assert_eq!(
            states(&report),
            [(A.to_string(), JobOutcomeState::Cancelled)]
        );
        assert!(
            started.elapsed() < STALL + STALL / 2,
            "unrelated events postponed the listing: {:?}",
            started.elapsed()
        );
        assert_eq!(source.lists(), 1);
    }

    /// B.5 #3: a lag marker from the daemon reconciles at once.
    #[tokio::test]
    async fn a_lag_marker_reconciles_at_once() {
        let source = FakeSource::new(vec![row(D, "done")], false, Vec::new());
        let rx = source.tx.subscribe();
        source.tx.send(lag_marker()).unwrap();
        let started = Instant::now();
        let report = complete(
            reconciling(&source, rx, quiet_view(&[D]), &[D])
                .await
                .unwrap(),
        );
        assert!(
            started.elapsed() < STALL / 2,
            "the lag marker waited for the deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(states(&report), [(D.to_string(), JobOutcomeState::Done)]);
    }

    /// B.5 #4: the daemon reporting itself degraded reconciles at once;
    /// the listing shows the failure that paused the queue and the rows
    /// left pending behind it.
    #[tokio::test]
    async fn a_degraded_daemon_reconciles_at_once() {
        let source = FakeSource::new(
            vec![
                row_with(A, "failed", "book", Some("out of file descriptors")),
                row(P, "pending"),
            ],
            true,
            Vec::new(),
        );
        let rx = source.tx.subscribe();
        source.tx.send(daemon_state("degraded")).unwrap();
        let started = Instant::now();
        let end = reconciling(&source, rx, quiet_view(&[A, P]), &[A, P])
            .await
            .unwrap();
        assert!(
            started.elapsed() < STALL / 2,
            "degraded waited for the deadline: {:?}",
            started.elapsed()
        );
        let (report, _) = refusal(&end);
        assert_eq!(states(report), [(A.to_string(), JobOutcomeState::Failed)]);
        assert_eq!(
            report.jobs[0].error.as_deref(),
            Some("out of file descriptors")
        );
        assert_eq!(report.unfinished, ids(&[P]));
    }

    /// B.5 #5: progress of an awaited job is what keeps the wait from
    /// listing; a job that keeps reporting is never reconciled.
    #[tokio::test]
    async fn progress_of_an_awaited_job_keeps_the_wait_from_listing() {
        let source = Arc::new(FakeSource::new(vec![row(A, "running")], false, Vec::new()));
        let rx = source.tx.subscribe();
        let reporter = {
            let source = Arc::clone(&source);
            tokio::spawn(async move {
                for _ in 0..6 {
                    tokio::time::sleep(STALL / 2).await;
                    let _ = source.tx.send(progress(A, "embed"));
                }
                let _ = source.tx.send(finished(A, "done", "book", None));
            })
        };
        let limit = Duration::from_secs(5);
        let end = tokio::time::timeout(
            limit,
            await_jobs_reconciling(
                &*source,
                rx,
                quiet_view(&[A]),
                Vec::new(),
                ids(&[A]),
                Instant::now(),
                STALL,
            ),
        )
        .await
        .expect("the wait did not complete")
        .unwrap();
        reporter.await.unwrap();
        assert_eq!(
            states(&complete(end)),
            [(A.to_string(), JobOutcomeState::Done)]
        );
        assert_eq!(source.lists(), 0, "a reporting job was listed");
    }

    /// B.5 #6: a paused queue with only a running job awaited is not a
    /// refusal; the wait lists once and keeps waiting for the outcome.
    #[tokio::test]
    async fn a_paused_queue_keeps_waiting_on_a_running_job() {
        let source = Arc::new(FakeSource::new(vec![row(R, "running")], true, Vec::new()));
        let rx = source.tx.subscribe();
        let finisher = {
            let source = Arc::clone(&source);
            tokio::spawn(async move {
                source.listed.notified().await;
                let _ = source.tx.send(finished(R, "done", "book", None));
            })
        };
        let report = complete(
            reconciling(&source, rx, quiet_view(&[R]), &[R])
                .await
                .unwrap(),
        );
        finisher.await.unwrap();
        assert_eq!(states(&report), [(R.to_string(), JobOutcomeState::Done)]);
        assert_eq!(source.lists(), 1);
    }

    /// B.5 #7: a listing that fails ends the wait as that failure,
    /// classified as the failure itself is.
    #[tokio::test]
    async fn a_listing_that_fails_ends_the_wait_as_that_failure() {
        let failures = [
            (
                ControlError::Transport(std::io::Error::other("connection reset")),
                None,
            ),
            (
                ControlError::Timeout {
                    method: "queue.list".to_string(),
                    after: Duration::from_secs(10),
                },
                None,
            ),
            (
                ControlError::Rpc {
                    code: bookrack_runtime::control::jsonrpc::BUSY,
                    message: "busy".to_string(),
                    data: None,
                },
                Some(4),
            ),
        ];
        for (failure, exit_code) in failures {
            let label = failure.to_string();
            let source =
                FakeSource::new(vec![row(P, "pending")], false, Vec::new()).failing_list(failure);
            let rx = source.tx.subscribe();
            let err = reconciling(&source, rx, quiet_view(&[P]), &[P])
                .await
                .unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains("queue.list") && message.contains("1 job"),
                "{label}: {message}"
            );
            assert_eq!(
                classify_eyre(&err).map(|cause| cause.as_cli().exit_code()),
                exit_code,
                "{label}: {err:?}"
            );
        }
    }

    /// B.5 #8: the local receiver falling behind reconciles at once.
    #[tokio::test]
    async fn a_lagged_local_receiver_reconciles_at_once() {
        let source = FakeSource::new(vec![row(D, "done")], false, Vec::new());
        let rx = source.tx.subscribe();
        for _ in 0..70 {
            source.tx.send(tick_without_finished()).unwrap();
        }
        let started = Instant::now();
        let report = complete(
            reconciling(&source, rx, quiet_view(&[D]), &[D])
                .await
                .unwrap(),
        );
        assert!(
            started.elapsed() < STALL / 2,
            "the lagged receiver waited for the deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(states(&report), [(D.to_string(), JobOutcomeState::Done)]);
    }

    /// B.5 #9: an awaited id the listing does not carry is a data
    /// error, not a refusal.
    #[tokio::test]
    async fn an_awaited_id_missing_from_the_listing_is_a_data_error() {
        let source = FakeSource::new(Vec::new(), false, Vec::new());
        let rx = source.tx.subscribe();
        let err = reconciling(&source, rx, quiet_view(&[A]), &[A])
            .await
            .unwrap_err();
        assert!(classify_eyre(&err).is_none(), "{err:?}");
        assert!(err.to_string().contains(A), "{err}");
    }

    /// B.5 #10: a deadline that has passed is seen before the events
    /// backlogged on the receiver, so a stream that is never empty
    /// cannot starve the listing.
    #[tokio::test]
    async fn an_expired_deadline_is_seen_before_backlogged_events() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let source =
            FakeSource::new(vec![row(P, "pending")], true, Vec::new()).logging_to(Arc::clone(&log));
        let rx = source.tx.subscribe();
        for _ in 0..16 {
            source.tx.send(progress(P, "processing")).unwrap();
        }
        let view = logged_view(&[P], &log);
        let end = tokio::time::timeout(
            LIMIT,
            await_jobs_reconciling(
                &source,
                rx,
                view,
                Vec::new(),
                ids(&[P]),
                Instant::now(),
                Duration::ZERO,
            ),
        )
        .await
        .expect("the wait did not reconcile")
        .unwrap();
        refusal(&end);
        let log = log.lock().unwrap();
        let listed_at = log
            .iter()
            .position(|line| line == "list")
            .expect("no listing");
        let first_progress = log.iter().position(|line| line.contains("processing"));
        assert!(
            first_progress.is_none_or(|at| listed_at < at),
            "backlogged events were drained before the expired deadline: {log:?}"
        );
    }

    /// B.5 #11: a reconciliation settles every ended row, with the
    /// record its outcome event would have given.
    #[tokio::test]
    async fn a_reconciliation_settles_every_ended_row() {
        let source = FakeSource::new(
            vec![
                row(A, "done"),
                row_with(B, "failed", "book", Some("boom")),
                row(D, "running"),
            ],
            false,
            Vec::new(),
        );
        let rx = source.tx.subscribe();
        source.tx.send(finished(D, "done", "book", None)).unwrap();
        let report = complete(
            reconciling(&source, rx, quiet_view(&[A, B, D]), &[A, B, D])
                .await
                .unwrap(),
        );
        assert_eq!(
            states(&report),
            [
                (D.to_string(), JobOutcomeState::Done),
                (A.to_string(), JobOutcomeState::Done),
                (B.to_string(), JobOutcomeState::Failed),
            ]
        );
        let pending: HashSet<String> = HashSet::from([B.to_string()]);
        let from_event =
            extract_finished(&finished(B, "failed", "book", Some("boom")).value, &pending).unwrap();
        assert_eq!(
            serde_json::to_value(&report.jobs[2]).unwrap(),
            serde_json::to_value(&from_event).unwrap()
        );
        assert_eq!(source.lists(), 1);
    }

    /// B.5 #12: a row the reconciliation settled is counted once when
    /// its outcome event arrives late, and the stage slot it held is
    /// freed as soon as it is settled.
    #[tokio::test]
    async fn a_row_settled_by_reconciliation_is_counted_once_and_frees_the_stage_slot() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(
            FakeSource::new(vec![row(A, "done"), row(B, "running")], false, Vec::new())
                .logging_to(Arc::clone(&log)),
        );
        let rx = source.tx.subscribe();
        source.tx.send(progress(A, "embed")).unwrap();
        let late = {
            let source = Arc::clone(&source);
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                source.listed.notified().await;
                // Settling A redraws the status line without A's stage.
                let deadline = Instant::now() + LIMIT;
                while !log
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|line| line == "status: 1/2 waiting")
                {
                    assert!(
                        Instant::now() < deadline,
                        "A kept the stage slot: {:?}",
                        log.lock().unwrap()
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let _ = source.tx.send(finished(A, "done", "book", None));
                let _ = source.tx.send(finished(B, "done", "book", None));
            })
        };
        let report = complete(
            reconciling(&source, rx, logged_view(&[A, B], &log), &[A, B])
                .await
                .unwrap(),
        );
        late.await.unwrap();
        assert_eq!(
            states(&report),
            [
                (A.to_string(), JobOutcomeState::Done),
                (B.to_string(), JobOutcomeState::Done),
            ]
        );
        let log = log.lock().unwrap();
        let listed_at = log.iter().position(|line| line == "list").unwrap();
        assert!(
            !log[listed_at..].iter().any(|line| line.contains("[embed")),
            "A's stage was drawn after it was settled: {log:?}"
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
