// SPDX-License-Identifier: Apache-2.0

//! `bookrack queue wait` — attach to queued work and report how it
//! ended.
//!
//! The wait subscribes to the event stream before it reads the queue,
//! so a job that ends after the read reaches the subscription and one
//! that ended before it reads as ended in the listing. That read fixes
//! the jobs the wait covers: every pending or running row, or the rows
//! `--job` names, each matched against every row in the queue. A named
//! row that has already ended is reported as it ended rather than
//! waited on. The worker and the pause flag are judged once, from the
//! same read; whatever changes afterwards is seen only through the
//! event stream.
//!
//! The report lists the ended rows first, in the order they were
//! selected, then the waited jobs in the order their outcomes arrived.

use std::time::{Duration, Instant};

use bookrack_cli::daemon_call::DEFAULT_AWAIT_STALL_TIMEOUT;
use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::human::basename_or_dash;
use bookrack_cli::render::job_report::{JobOutcomeRecord, JobOutcomeReport, JobOutcomeState};
use bookrack_cli::render::progress::{Surface, SurfaceKind};
use bookrack_cli::render::{OutputMode, ctx};
use bookrack_control_client::{ControlClient, Event};
use bookrack_core::Problem;
use bookrack_core::queue::{FragmentMiss, JobState, QueueJob, match_id_fragment};
use eyre::{Context, Result, eyre};
use serde_json::Value;
use tokio::sync::broadcast;

use super::helpers;
use super::progress_view::ProgressView;

const FULL_ID_HINT: &str = "`bookrack queue list --long` prints every job's full id.";

/// The daemon calls a queue wait makes.
pub(crate) trait WaitSource {
    async fn subscribe(&self) -> Result<broadcast::Receiver<Event>>;
    async fn list(&self) -> Result<Value>;
    async fn worker_enabled(&self) -> Result<bool>;
}

/// [`WaitSource`] over a live control-plane connection.
struct Daemon<'a>(&'a ControlClient);

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

/// How a wait ended.
#[derive(Debug)]
pub(crate) enum WaitResult {
    /// No job was pending or running, and none was named.
    Nothing,
    /// The jobs the wait covered, and the noun the summary line names
    /// them by.
    Report {
        report: JobOutcomeReport,
        label: String,
    },
}

/// `bookrack queue wait`: wait on the selected jobs and print how they
/// ended, with the summary line and exit code `ingest --wait` uses.
pub async fn run(client: &ControlClient, fragments: &[String]) -> Result<()> {
    let surface = SurfaceKind::from_context().build();
    match wait_on(
        &Daemon(client),
        fragments,
        surface,
        DEFAULT_AWAIT_STALL_TIMEOUT,
    )
    .await?
    {
        WaitResult::Nothing => {
            if let Some(text) = render_nothing(ctx().output()) {
                println!("{text}");
            }
            Ok(())
        }
        WaitResult::Report { report, label } => {
            helpers::finalize_job_batch(&report, "queue wait", "Completed", &label)
        }
    }
}

/// What a wait with nothing to wait on prints: one line for a person,
/// an empty report under `--json`, nothing under `--quiet`.
fn render_nothing(mode: OutputMode) -> Option<String> {
    match mode {
        OutputMode::Human => Some("nothing to wait for".to_string()),
        OutputMode::Json => {
            serde_json::to_string_pretty(&JobOutcomeReport::new(Vec::new(), Duration::ZERO)).ok()
        }
        OutputMode::Quiet => None,
    }
}

/// Subscribe, read the queue, select and judge the jobs, then wait on
/// the live ones. `stall` bounds the stretch between two events.
pub(crate) async fn wait_on<S: WaitSource>(
    source: &S,
    fragments: &[String],
    surface: Box<dyn Surface>,
    stall: Duration,
) -> Result<WaitResult> {
    let started = Instant::now();
    let rx = source.subscribe().await?;
    let listing = Listing::parse(&source.list().await?)?;
    let selected = select(&listing.jobs, fragments)?;
    if selected.is_empty() {
        return Ok(WaitResult::Nothing);
    }

    let mut settled = Vec::new();
    let mut live = Vec::new();
    for &index in &selected {
        let job = &listing.jobs[index];
        if is_live(job.state) {
            live.push(job);
        } else {
            settled.push(ended_record(job, &listing.rows[index])?);
        }
    }

    if !live.is_empty() {
        if !source.worker_enabled().await? {
            return Err(no_worker(live.len()).into());
        }
        let pending = live
            .iter()
            .filter(|job| job.state == JobState::Pending)
            .count();
        if listing.paused && pending > 0 {
            return Err(paused(pending).into());
        }
    }

    let label = match selected.as_slice() {
        [only] => basename_or_dash(listing.jobs[*only].path.to_str()).to_string(),
        _ => "jobs".to_string(),
    };
    let live_ids: Vec<String> = live.iter().map(|job| job.id.clone()).collect();
    let view = ProgressView::for_jobs_after(&settled, &live_ids, surface);
    let awaited = helpers::await_jobs_from_rx(rx, view, started, stall).await?;
    settled.extend(awaited.jobs);
    Ok(WaitResult::Report {
        report: JobOutcomeReport::new(settled, started.elapsed()),
        label,
    })
}

/// A `queue.list` reply: the typed rows, the same rows as sent, and the
/// pause flag.
struct Listing {
    jobs: Vec<QueueJob>,
    rows: Vec<Value>,
    paused: bool,
}

impl Listing {
    fn parse(reply: &Value) -> Result<Self> {
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

fn is_live(state: JobState) -> bool {
    matches!(state, JobState::Pending | JobState::Running)
}

/// Indices of the rows a wait covers: every pending or running row in
/// queue order, or the rows `fragments` name in the order they were
/// named. A fragment is matched against every row, ended ones
/// included, and a row named twice counts once.
fn select(jobs: &[QueueJob], fragments: &[String]) -> Result<Vec<usize>, BookrackCliError> {
    if fragments.is_empty() {
        return Ok((0..jobs.len())
            .filter(|&i| is_live(jobs[i].state))
            .collect());
    }
    let mut picked = Vec::new();
    for fragment in fragments {
        let index = match_id_fragment(jobs, fragment, |_| true)
            .map_err(|miss| fragment_refusal(fragment, miss))?;
        if !picked.contains(&index) {
            picked.push(index);
        }
    }
    Ok(picked)
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

fn refused(problem: Problem) -> BookrackCliError {
    BookrackCliError::QueueWaitRefused { problem }
}

fn fragment_refusal(fragment: &str, miss: FragmentMiss) -> BookrackCliError {
    refused(match miss {
        FragmentMiss::Empty => Problem::new("a job id fragment must not be empty"),
        FragmentMiss::NoMatch => {
            Problem::new(format!("no job matches {fragment:?}")).hint(FULL_ID_HINT)
        }
        FragmentMiss::Ambiguous { count } => {
            Problem::new(format!("{fragment:?} is ambiguous, matches {count} jobs"))
                .hint(format!("Pass more of the id; {FULL_ID_HINT}"))
        }
    })
}

fn no_worker(live: usize) -> BookrackCliError {
    let detail = if live == 1 {
        "1 selected job is pending or running, and nothing will run it.".to_string()
    } else {
        format!("{live} selected jobs are pending or running, and nothing will run them.")
    };
    refused(
        Problem::new("cannot wait on queue jobs: the daemon has no queue worker")
            .detail(detail)
            .hint("Restart the daemon with a queue worker: `bookrack run`."),
    )
}

fn paused(pending: usize) -> BookrackCliError {
    let detail = if pending == 1 {
        "1 selected job is pending; a paused worker pulls no job.".to_string()
    } else {
        format!("{pending} selected jobs are pending; a paused worker pulls no job.")
    };
    refused(
        Problem::new("cannot wait on pending jobs while the queue is paused")
            .detail(detail)
            .hint("Run `bookrack queue resume`, then wait again."),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use bookrack_cli::error::classify_eyre;
    use serde_json::json;

    use super::super::progress_view::extract_finished;
    use super::super::progress_view::testing::RecordingSurface;
    use super::*;

    // Ids as one minute's submissions generate them: a shared timestamp
    // head, distinct random tails.
    const A: &str = "019a1b2c-3d4e-7f00-8a11-0000aaaaaaaa";
    const B: &str = "019a1b2c-3d4e-7f00-8a11-0000bbbbbbbb";
    const D: &str = "019a1b2c-3d4e-7f00-8a11-0000dddddddd";
    const N: &str = "019a1b2c-3d4e-7f00-8a11-0000eeeeeeee";
    const P: &str = "019a1b2c-3d4e-7f00-8a11-000011111111";
    const R: &str = "019a1b2c-3d4e-7f00-8a11-000022222222";
    const R1: &str = "019a1b2c-3d4e-7f00-8a11-000033333333";
    const R2: &str = "019a1b2c-3d4e-7f00-8a11-000044444444";

    const FINISHED_AT: &str = "2026-01-02T03:05:05.123456789Z";
    const STALL: Duration = Duration::from_secs(2);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        Subscribe,
        List,
        WorkerEnabled,
    }

    /// A daemon whose `queue.list` reply and worker flag are fixed. The
    /// events in `on_list` are broadcast inside `list`, before the reply
    /// returns, so they reach only a subscription made earlier.
    struct FakeSource {
        tx: broadcast::Sender<Event>,
        calls: Mutex<Vec<Call>>,
        reply: Value,
        worker: bool,
        on_list: Vec<Event>,
    }

    impl FakeSource {
        fn new(rows: Vec<Value>, paused: bool, on_list: Vec<Event>) -> Self {
            let (tx, _) = broadcast::channel(64);
            Self {
                tx,
                calls: Mutex::new(Vec::new()),
                reply: json!({ "paused": paused, "jobs": rows }),
                worker: true,
                on_list,
            }
        }

        fn without_worker(mut self) -> Self {
            self.worker = false;
            self
        }

        fn calls(&self) -> Vec<Call> {
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

    fn row(id: &str, state: &str) -> Value {
        row_with(id, state, "book", None)
    }

    fn row_with(id: &str, state: &str, kind: &str, error: Option<&str>) -> Value {
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

    fn finished(id: &str, state: &str, kind: &str, error: Option<&str>) -> Event {
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

    fn hidden() -> Box<dyn Surface> {
        SurfaceKind::Hidden.build()
    }

    fn named(fragments: &[&str]) -> Vec<String> {
        fragments.iter().map(|f| f.to_string()).collect()
    }

    fn outcomes(result: WaitResult) -> Vec<(String, JobOutcomeState)> {
        let WaitResult::Report { report, .. } = result else {
            panic!("expected a report, got {result:?}");
        };
        report
            .jobs
            .into_iter()
            .map(|job| (job.job_id, job.state))
            .collect()
    }

    fn refusal(err: &eyre::Report) -> &Problem {
        match err.downcast_ref::<BookrackCliError>() {
            Some(BookrackCliError::QueueWaitRefused { problem }) => problem,
            other => panic!("expected a queue wait refusal, got {other:?}: {err:?}"),
        }
    }

    #[tokio::test]
    async fn subscribes_before_listing_and_keeps_a_finish_sent_before_the_list_reply() {
        let source = FakeSource::new(
            vec![row(R, "running")],
            false,
            vec![finished(R, "done", "book", None)],
        );
        let result = wait_on(&source, &[], hidden(), STALL).await.unwrap();
        assert_eq!(&source.calls()[..2], [Call::Subscribe, Call::List]);
        assert_eq!(outcomes(result), [(R.to_string(), JobOutcomeState::Done)]);
    }

    #[tokio::test]
    async fn a_named_job_that_already_ended_is_reported_without_waiting() {
        let source = FakeSource::new(
            vec![row_with(A, "failed", "book", Some("boom")), row(B, "done")],
            false,
            Vec::new(),
        );
        let result = wait_on(&source, &named(&["aaaaaaaa", "bbbbbbbb"]), hidden(), STALL)
            .await
            .unwrap();
        let WaitResult::Report { report, label } = result else {
            panic!("expected a report");
        };
        assert_eq!(label, "jobs");
        assert_eq!(report.jobs.len(), 2);
        assert_eq!(report.jobs[0].job_id, A);
        assert_eq!(report.jobs[0].error.as_deref(), Some("boom"));
        assert_eq!(report.jobs[1].state, JobOutcomeState::Done);
        assert_eq!(report.totals.failed, 1);
        assert!(!report.all_succeeded());
    }

    #[tokio::test]
    async fn ended_and_live_named_jobs_merge_into_one_report() {
        let source = FakeSource::new(
            vec![
                row_with(A, "failed", "book", Some("boom")),
                row(R, "running"),
            ],
            false,
            vec![finished(R, "done", "book", None)],
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let result = wait_on(
            &source,
            &named(&[A, R]),
            Box::new(RecordingSurface(Arc::clone(&log))),
            STALL,
        )
        .await
        .unwrap();
        assert_eq!(
            outcomes(result),
            [
                (A.to_string(), JobOutcomeState::Failed),
                (R.to_string(), JobOutcomeState::Done),
            ]
        );
        let notices: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.starts_with("notice: "))
            .cloned()
            .collect();
        assert_eq!(
            notices,
            [
                "notice: 1/2 aaaaaaaa failed: boom",
                "notice: 2/2 22222222 done",
            ]
        );
    }

    #[tokio::test]
    async fn fragments_match_every_row_and_duplicates_collapse() {
        let source = FakeSource::new(vec![row(D, "done"), row(P, "pending")], false, Vec::new());
        let result = wait_on(&source, &named(&["dddd", D]), hidden(), STALL)
            .await
            .unwrap();
        assert_eq!(outcomes(result), [(D.to_string(), JobOutcomeState::Done)]);
    }

    #[tokio::test]
    async fn an_unmatched_or_ambiguous_fragment_is_refused_before_any_check() {
        let rows = vec![row(A, "pending"), row(B, "pending")];

        let source = FakeSource::new(rows.clone(), false, Vec::new());
        let err = wait_on(&source, &named(&["zz"]), hidden(), STALL)
            .await
            .unwrap_err();
        assert_eq!(refusal(&err).summary, "no job matches \"zz\"");
        assert_eq!(classify_eyre(&err).unwrap().as_cli().exit_code(), 2);
        assert!(!source.calls().contains(&Call::WorkerEnabled));

        let source = FakeSource::new(rows, false, Vec::new());
        let err = wait_on(&source, &named(&["019a1b2c"]), hidden(), STALL)
            .await
            .unwrap_err();
        assert_eq!(
            refusal(&err).summary,
            "\"019a1b2c\" is ambiguous, matches 2 jobs"
        );
        assert!(!source.calls().contains(&Call::WorkerEnabled));
    }

    #[tokio::test]
    async fn a_paused_queue_refuses_selected_pending_jobs() {
        let source = FakeSource::new(vec![row(P, "pending"), row(R, "running")], true, Vec::new());
        let err = wait_on(&source, &[], hidden(), STALL).await.unwrap_err();
        let problem = refusal(&err);
        assert_eq!(
            problem.summary,
            "cannot wait on pending jobs while the queue is paused"
        );
        assert!(
            problem
                .data
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("bookrack queue resume")),
            "{problem:?}"
        );
    }

    #[tokio::test]
    async fn a_paused_queue_still_waits_on_running_jobs() {
        let source = FakeSource::new(
            vec![row(P, "pending"), row(R, "running")],
            true,
            vec![finished(R, "done", "book", None)],
        );
        let result = wait_on(&source, &named(&[R]), hidden(), STALL)
            .await
            .unwrap();
        assert_eq!(outcomes(result), [(R.to_string(), JobOutcomeState::Done)]);
    }

    #[tokio::test]
    async fn a_daemon_without_a_worker_refuses_live_jobs_but_reports_ended_ones() {
        let source = FakeSource::new(vec![row(P, "pending"), row(D, "done")], false, Vec::new())
            .without_worker();
        let err = wait_on(&source, &[], hidden(), STALL).await.unwrap_err();
        assert_eq!(
            refusal(&err).summary,
            "cannot wait on queue jobs: the daemon has no queue worker"
        );

        let source = FakeSource::new(vec![row(P, "pending"), row(D, "done")], false, Vec::new())
            .without_worker();
        let result = wait_on(&source, &named(&[D]), hidden(), STALL)
            .await
            .unwrap();
        assert_eq!(outcomes(result), [(D.to_string(), JobOutcomeState::Done)]);
        assert!(!source.calls().contains(&Call::WorkerEnabled));
    }

    #[tokio::test]
    async fn nothing_selected_is_nothing_to_wait_for() {
        let source = FakeSource::new(vec![row(D, "done")], false, Vec::new());
        let result = wait_on(&source, &[], hidden(), STALL).await.unwrap();
        assert!(matches!(result, WaitResult::Nothing), "{result:?}");

        assert_eq!(
            render_nothing(OutputMode::Human).as_deref(),
            Some("nothing to wait for")
        );
        assert_eq!(render_nothing(OutputMode::Quiet), None);
        let json: Value = serde_json::from_str(&render_nothing(OutputMode::Json).unwrap()).unwrap();
        assert_eq!(json["jobs"], json!([]));
        assert!(
            json["totals"]
                .as_object()
                .unwrap()
                .values()
                .all(|count| count == 0),
            "{json}"
        );
    }

    /// A row that never reached the wait set would surface as a stall
    /// after `STALL`; the data error names the row and returns at once.
    #[tokio::test]
    async fn an_ended_row_without_finished_at_is_a_data_error() {
        let mut broken = row_with(A, "failed", "book", Some("boom"));
        broken["finished_at"] = Value::Null;
        let source = FakeSource::new(vec![broken], false, Vec::new());
        let err = wait_on(&source, &named(&[A]), hidden(), STALL)
            .await
            .unwrap_err();
        assert!(classify_eyre(&err).is_none(), "{err:?}");
        let message = err.to_string();
        assert!(
            message.contains(A) && message.contains("finished_at"),
            "{message}"
        );
    }

    /// Covers a stale finish of another job only. A stale finish of a
    /// job retried between the subscription and the read is not
    /// filtered; that is an accepted limit of this wait.
    #[tokio::test]
    async fn the_default_set_ignores_ended_rows_and_stale_finishes_of_other_jobs() {
        let source = FakeSource::new(
            vec![row(D, "done"), row(P, "pending")],
            false,
            vec![
                finished(D, "done", "book", None),
                finished(P, "done", "book", None),
            ],
        );
        let result = wait_on(&source, &[], hidden(), STALL).await.unwrap();
        assert_eq!(outcomes(result), [(P.to_string(), JobOutcomeState::Done)]);
    }

    #[tokio::test]
    async fn the_report_lists_ended_jobs_first_then_in_completion_order() {
        let source = FakeSource::new(
            vec![
                row(R1, "running"),
                row(R2, "running"),
                row_with(A, "failed", "book", Some("boom")),
            ],
            false,
            vec![
                finished(R2, "done", "book", None),
                finished(R1, "failed", "book", Some("late")),
            ],
        );
        let result = wait_on(&source, &named(&[R1, R2, A]), hidden(), STALL)
            .await
            .unwrap();
        assert_eq!(
            outcomes(result),
            [
                (A.to_string(), JobOutcomeState::Failed),
                (R2.to_string(), JobOutcomeState::Done),
                (R1.to_string(), JobOutcomeState::Failed),
            ]
        );
    }

    #[tokio::test]
    async fn the_wait_set_is_fixed_when_the_queue_is_read() {
        let queued_later = Event {
            channel: "worker.progress".to_string(),
            value: json!({ "job_id": N, "kind": "book", "stage": "processing" }),
            lag: false,
        };
        let more_pending = Event {
            channel: "queue.tick".to_string(),
            value: json!({ "pending": 2, "running": 0 }),
            lag: false,
        };
        let source = FakeSource::new(
            vec![row(P, "pending")],
            false,
            vec![
                more_pending,
                queued_later,
                finished(P, "done", "book", None),
            ],
        );
        let result = wait_on(&source, &[], hidden(), STALL).await.unwrap();
        assert_eq!(outcomes(result), [(P.to_string(), JobOutcomeState::Done)]);
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
