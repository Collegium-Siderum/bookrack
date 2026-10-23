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
use bookrack_cli::render::job_report::JobOutcomeReport;
use bookrack_cli::render::progress::{Surface, SurfaceKind};
use bookrack_cli::render::{OutputMode, ctx};
use bookrack_control_client::ControlClient;
use bookrack_core::Problem;
use bookrack_core::queue::{FragmentMiss, QueueJob, match_id_fragment};
use eyre::Result;

use super::job_wait::{self, Daemon, Listing, WaitEnd, WaitSource, is_live};

const FULL_ID_HINT: &str = "`bookrack queue list --long` prints every job's full id.";

/// How a wait ended, with the noun the summary line names the jobs by.
#[derive(Debug)]
pub(super) enum WaitResult {
    /// No job was pending or running, and none was named.
    Nothing,
    /// The jobs the wait covered.
    Ended { end: WaitEnd, label: String },
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
        WaitResult::Ended { end, label } => {
            job_wait::finalize(end, "queue wait", "Completed", &label)
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
pub(super) async fn wait_on<S: WaitSource>(
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
    let label = match selected.as_slice() {
        [only] => basename_or_dash(listing.jobs[*only].path.to_str()).to_string(),
        _ => "jobs".to_string(),
    };
    let attached = job_wait::attach(&listing, &selected)?;
    let end = job_wait::wait(source, rx, &listing, attached, surface, started, stall).await?;
    Ok(WaitResult::Ended { end, label })
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

fn fragment_refusal(fragment: &str, miss: FragmentMiss) -> BookrackCliError {
    job_wait::refused(match miss {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bookrack_cli::error::classify_eyre;
    use bookrack_cli::render::job_report::JobOutcomeState;
    use serde_json::{Value, json};

    use super::super::job_wait::testing::*;
    use super::super::progress_view::testing::RecordingSurface;
    use super::*;

    const STALL: Duration = Duration::from_secs(2);

    fn named(fragments: &[&str]) -> Vec<String> {
        ids(fragments)
    }

    /// The complete report of a wait that ended, as `(id, state)` pairs.
    fn outcomes(result: WaitResult) -> Vec<(String, JobOutcomeState)> {
        let WaitResult::Ended {
            end: WaitEnd::Complete(report),
            ..
        } = result
        else {
            panic!("expected a complete report, got {result:?}");
        };
        report
            .jobs
            .into_iter()
            .map(|job| (job.job_id, job.state))
            .collect()
    }

    /// The partial report and problem of a wait refused after the read.
    fn refusal(result: &WaitResult) -> (&JobOutcomeReport, &Problem) {
        match result {
            WaitResult::Ended {
                end: WaitEnd::Refused { report, problem },
                ..
            } => (report, problem),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The problem of a wait refused before the read.
    fn early_refusal(err: &eyre::Report) -> &Problem {
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
        let WaitResult::Ended {
            end: WaitEnd::Complete(report),
            label,
        } = result
        else {
            panic!("expected a complete report");
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
        assert_eq!(early_refusal(&err).summary, "no job matches \"zz\"");
        assert_eq!(classify_eyre(&err).unwrap().as_cli().exit_code(), 2);
        assert!(!source.calls().contains(&Call::WorkerEnabled));

        let source = FakeSource::new(rows, false, Vec::new());
        let err = wait_on(&source, &named(&["019a1b2c"]), hidden(), STALL)
            .await
            .unwrap_err();
        assert_eq!(
            early_refusal(&err).summary,
            "\"019a1b2c\" is ambiguous, matches 2 jobs"
        );
        assert!(!source.calls().contains(&Call::WorkerEnabled));
    }

    /// The refusal reports the selected rows: none had ended, both are
    /// named under `unfinished`, in queue order.
    #[tokio::test]
    async fn a_paused_queue_refuses_selected_pending_jobs() {
        let source = FakeSource::new(vec![row(P, "pending"), row(R, "running")], true, Vec::new());
        let result = wait_on(&source, &[], hidden(), STALL).await.unwrap();
        let (report, problem) = refusal(&result);
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
        assert!(report.jobs.is_empty());
        assert_eq!(report.unfinished, ids(&[P, R]));
    }

    /// A refusal keeps the ended rows it was asked about, in the order
    /// they were named, and lists the live ones after them.
    #[tokio::test]
    async fn a_paused_queue_refusal_reports_the_named_ended_rows() {
        let source = FakeSource::new(
            vec![
                row(P, "pending"),
                row_with(A, "failed", "book", Some("boom")),
                row(R, "running"),
                row(D, "done"),
            ],
            true,
            Vec::new(),
        );
        let result = wait_on(&source, &named(&[D, R, A, P]), hidden(), STALL)
            .await
            .unwrap();
        let (report, _) = refusal(&result);
        let seen: Vec<&str> = report.jobs.iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(seen, [D, A]);
        assert_eq!(report.unfinished, ids(&[R, P]));
        assert_eq!(report.totals.failed, 1);
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

    /// The default selection covers live rows only, so the refusal
    /// names the pending row and reports nothing ended; the ended row
    /// is reported when it is selected by name, alone or beside the
    /// pending one.
    #[tokio::test]
    async fn a_daemon_without_a_worker_refuses_live_jobs_but_reports_ended_ones() {
        let source = FakeSource::new(vec![row(P, "pending"), row(D, "done")], false, Vec::new())
            .without_worker();
        let result = wait_on(&source, &[], hidden(), STALL).await.unwrap();
        let (report, problem) = refusal(&result);
        assert_eq!(
            problem.summary,
            "cannot wait on queue jobs: the daemon has no queue worker"
        );
        assert!(report.jobs.is_empty());
        assert_eq!(report.unfinished, ids(&[P]));

        let source = FakeSource::new(vec![row(P, "pending"), row(D, "done")], false, Vec::new())
            .without_worker();
        let result = wait_on(&source, &named(&[D]), hidden(), STALL)
            .await
            .unwrap();
        assert_eq!(outcomes(result), [(D.to_string(), JobOutcomeState::Done)]);
        assert!(!source.calls().contains(&Call::WorkerEnabled));

        let source = FakeSource::new(vec![row(P, "pending"), row(D, "done")], false, Vec::new())
            .without_worker();
        let result = wait_on(&source, &named(&[P, D]), hidden(), STALL)
            .await
            .unwrap();
        let (report, _) = refusal(&result);
        assert_eq!(report.jobs.len(), 1);
        assert_eq!(report.jobs[0].job_id, D);
        assert_eq!(report.unfinished, ids(&[P]));
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
        assert!(json.get("unfinished").is_none(), "{json}");
        assert!(
            json["totals"]
                .as_object()
                .unwrap()
                .values()
                .all(|count| count == 0),
            "{json}"
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
        let queued_later = bookrack_control_client::Event {
            channel: "worker.progress".to_string(),
            value: json!({ "job_id": N, "kind": "book", "stage": "processing" }),
            lag: false,
        };
        let more_pending = bookrack_control_client::Event {
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
}
