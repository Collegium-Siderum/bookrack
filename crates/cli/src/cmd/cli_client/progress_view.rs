// SPDX-License-Identifier: Apache-2.0

//! Progress line for the CLI's two control-plane wait paths.
//!
//! [`ProgressView`] folds broadcast events into a [`ViewState`] and
//! draws it on a [`Surface`] chosen from the output mode and whether
//! stderr is a terminal. A view built for a batch of queue jobs counts
//! and labels only that batch. A view built for one RPC call shows the
//! method next to a spinner and draws no queue activity: a call
//! publishes no `worker.progress` or `queue.tick` of its own, so any
//! that arrive belong to unrelated jobs. Dropping the view clears what
//! it drew, on every exit from the wait.

use std::collections::HashSet;

use bookrack_cli::render::human::short_id;
use bookrack_cli::render::job_report::{JobOutcomeRecord, JobOutcomeState, state_word};
use bookrack_cli::render::progress::{Surface, SurfaceKind};
use bookrack_control_client::Event;
use serde_json::Value;

const LAG_NOTICE: &str = "bookrack: event stream lagged; progress may be incomplete";

/// The stage a batch job was last reported in on `worker.progress`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageSlot {
    pub job_id: String,
    pub stage: String,
    /// Whole percent of the stage, present only when the event carried
    /// a fraction inside `0.0..=1.0`.
    pub percent: Option<u8>,
}

#[derive(Debug)]
enum Scope {
    /// Waiting on a batch of queue jobs. `pending` starts as the
    /// deduplicated batch and loses each job once its terminal state
    /// is seen; `total` is the size of that deduplicated batch.
    Jobs {
        total: usize,
        pending: HashSet<String>,
    },
    /// Waiting on one RPC call.
    Call { method: String },
}

/// What folding one event into a [`ViewState`] changed.
#[derive(Debug, Default)]
pub(crate) struct Applied {
    /// Terminal record of a batch job the event finished.
    pub finished: Option<JobOutcomeRecord>,
    /// A line to print above the status line.
    pub notice: Option<String>,
    /// The stage slot now names a different job or stage.
    pub stage_changed: bool,
}

/// Drawing-free state of a [`ProgressView`].
#[derive(Debug)]
pub(crate) struct ViewState {
    scope: Scope,
    slot: Option<StageSlot>,
}

impl ViewState {
    /// State for a wait on the queue jobs in `job_ids`. A repeated id
    /// counts once.
    pub fn for_jobs(job_ids: &[String]) -> Self {
        Self::for_jobs_after(&[], job_ids)
    }

    /// State for a wait on the queue jobs in `live` that also reports
    /// `settled`, jobs that had already ended when the wait began. The
    /// ended jobs count as finished from the start. A repeated live id
    /// counts once.
    pub fn for_jobs_after(settled: &[JobOutcomeRecord], live: &[String]) -> Self {
        let pending: HashSet<String> = live.iter().cloned().collect();
        Self {
            scope: Scope::Jobs {
                total: settled.len() + pending.len(),
                pending,
            },
            slot: None,
        }
    }

    /// State for a wait on one RPC call to `method`.
    pub fn for_call(method: &str) -> Self {
        Self {
            scope: Scope::Call {
                method: method.to_string(),
            },
            slot: None,
        }
    }

    /// `(finished, total)` for a batch; `None` for a call.
    pub fn counts(&self) -> Option<(usize, usize)> {
        match &self.scope {
            Scope::Jobs { total, pending } => Some((total - pending.len(), *total)),
            Scope::Call { .. } => None,
        }
    }

    /// Number of batch jobs without a terminal state yet; zero for a
    /// call.
    pub fn pending(&self) -> usize {
        match &self.scope {
            Scope::Jobs { pending, .. } => pending.len(),
            Scope::Call { .. } => 0,
        }
    }

    /// True once every job of a batch reached a terminal state. A call
    /// is never complete here; its wait ends when the call returns.
    pub fn is_complete(&self) -> bool {
        matches!(&self.scope, Scope::Jobs { pending, .. } if pending.is_empty())
    }

    /// Folds one broadcast event into the state.
    ///
    /// A batch takes `worker.progress` and terminal `queue.tick`
    /// outcomes for its own jobs only. A call takes neither. A lagged
    /// stream becomes a notice in both.
    pub fn apply(&mut self, event: &Event) -> Applied {
        if event.lag {
            return Applied {
                notice: Some(LAG_NOTICE.to_string()),
                ..Applied::default()
            };
        }
        let Self { scope, slot } = self;
        let Scope::Jobs { total, pending } = scope else {
            return Applied::default();
        };
        match event.channel.as_str() {
            "worker.progress" => {
                let value = &event.value;
                let Some(job_id) = value.get("job_id").and_then(Value::as_str) else {
                    return Applied::default();
                };
                let Some(stage) = value.get("stage").and_then(Value::as_str) else {
                    return Applied::default();
                };
                if !pending.contains(job_id) {
                    return Applied::default();
                }
                let percent = value
                    .get("stage_progress")
                    .and_then(Value::as_f64)
                    .and_then(whole_percent);
                let stage_changed = slot
                    .as_ref()
                    .is_none_or(|s| s.job_id != job_id || s.stage != stage);
                *slot = Some(StageSlot {
                    job_id: job_id.to_string(),
                    stage: stage.to_string(),
                    percent,
                });
                Applied {
                    stage_changed,
                    ..Applied::default()
                }
            }
            "queue.tick" => {
                let Some(record) = extract_finished(&event.value, pending) else {
                    return Applied::default();
                };
                pending.remove(&record.job_id);
                if slot.as_ref().is_some_and(|s| s.job_id == record.job_id) {
                    *slot = None;
                }
                let done = *total - pending.len();
                let notice = (*total > 1).then(|| outcome_line(done, *total, &record));
                Applied {
                    finished: Some(record),
                    notice,
                    stage_changed: false,
                }
            }
            _ => Applied::default(),
        }
    }

    /// Line drawn next to the spinner.
    pub fn status_line(&self) -> String {
        match &self.scope {
            Scope::Call { method } => method.clone(),
            Scope::Jobs { total, pending } => {
                let done = total - pending.len();
                match &self.slot {
                    Some(s) => format!(
                        "{done}/{total} {} [{} {}]",
                        short_id(&s.job_id),
                        s.stage,
                        percent_cell(s.percent)
                    ),
                    None => format!("{done}/{total} waiting"),
                }
            }
        }
    }

    /// Line recording the current stage, for a surface that appends
    /// instead of redrawing. `None` when no batch job holds the slot.
    pub fn transition_line(&self) -> Option<String> {
        let (done, total) = self.counts()?;
        let s = self.slot.as_ref()?;
        Some(format!(
            "{done}/{total} {} [{}]",
            short_id(&s.job_id),
            s.stage
        ))
    }
}

/// Parses a `last_finished` payload off a `queue.tick` value into a
/// [`JobOutcomeRecord`]. Returns `None` when the tick has no terminal
/// outcome, when the id is not one of the awaited jobs, or when any
/// required field is missing or unknown.
pub(crate) fn extract_finished(
    value: &Value,
    pending: &HashSet<String>,
) -> Option<JobOutcomeRecord> {
    let lf = value.get("last_finished")?;
    let job_id = lf.get("job_id")?.as_str()?.to_string();
    if !pending.contains(&job_id) {
        return None;
    }
    let kind = lf.get("kind")?.as_str()?.to_string();
    let state = JobOutcomeState::from_wire(lf.get("state")?.as_str()?)?;
    let error = lf.get("error").and_then(Value::as_str).map(String::from);
    let finished_at = lf.get("finished_at")?.as_str()?.to_string();
    Some(JobOutcomeRecord {
        job_id,
        kind,
        state,
        error,
        finished_at,
    })
}

fn whole_percent(fraction: f64) -> Option<u8> {
    (0.0..=1.0)
        .contains(&fraction)
        .then(|| (fraction * 100.0).round() as u8)
}

fn percent_cell(percent: Option<u8>) -> String {
    percent
        .map(|p| format!("{p:>3}%"))
        .unwrap_or_else(|| "----".to_string())
}

fn outcome_line(done: usize, total: usize, record: &JobOutcomeRecord) -> String {
    let mut line = format!(
        "{done}/{total} {} {}",
        short_id(&record.job_id),
        state_word(record.state)
    );
    if let Some(error) = &record.error {
        line.push_str(": ");
        line.push_str(error);
    }
    line
}

/// A [`ViewState`] drawn on a [`Surface`]. Dropping it clears the
/// surface.
pub(crate) struct ProgressView {
    state: ViewState,
    surface: Box<dyn Surface>,
}

impl ProgressView {
    /// View for a wait on the queue jobs in `job_ids`, drawn on the
    /// surface the render context selects.
    pub fn for_jobs(job_ids: &[String]) -> Self {
        Self::with_surface(ViewState::for_jobs(job_ids), default_surface())
    }

    /// View for a wait on one RPC call to `method`, drawn on the
    /// surface the render context selects.
    pub fn for_call(method: &str) -> Self {
        Self::with_surface(ViewState::for_call(method), default_surface())
    }

    /// View for a wait on the queue jobs in `live` that also reports
    /// `settled`, drawn on `surface`. Each ended job gets its outcome
    /// line up front, numbered before the live ones, when the batch
    /// holds more than one job.
    pub fn for_jobs_after(
        settled: &[JobOutcomeRecord],
        live: &[String],
        surface: Box<dyn Surface>,
    ) -> Self {
        let mut view = Self::with_surface(ViewState::for_jobs_after(settled, live), surface);
        if let Some((_, total)) = view.state.counts()
            && total > 1
        {
            for (index, record) in settled.iter().enumerate() {
                view.surface.notice(&outcome_line(index + 1, total, record));
            }
        }
        view
    }

    pub fn with_surface(state: ViewState, mut surface: Box<dyn Surface>) -> Self {
        surface.status(&state.status_line());
        Self { state, surface }
    }

    pub fn is_complete(&self) -> bool {
        self.state.is_complete()
    }

    pub fn pending(&self) -> usize {
        self.state.pending()
    }

    /// Folds one event into the view and redraws it. Returns the
    /// terminal record of a batch job the event finished.
    pub fn apply(&mut self, event: &Event) -> Option<JobOutcomeRecord> {
        let applied = self.state.apply(event);
        if let Some(notice) = &applied.notice {
            self.surface.notice(notice);
        }
        if applied.stage_changed
            && let Some(line) = self.state.transition_line()
        {
            self.surface.transition(&line);
        }
        self.surface.status(&self.state.status_line());
        applied.finished
    }
}

impl Drop for ProgressView {
    fn drop(&mut self) {
        self.surface.clear();
    }
}

fn default_surface() -> Box<dyn Surface> {
    SurfaceKind::from_context().build()
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, Mutex};

    use super::Surface;

    /// Surface that records every call as `"<method>: <line>"`, or
    /// `"clear"`, in order.
    pub(crate) struct RecordingSurface(pub Arc<Mutex<Vec<String>>>);

    impl RecordingSurface {
        fn push(&self, entry: String) {
            self.0.lock().expect("recording lock").push(entry);
        }
    }

    impl Surface for RecordingSurface {
        fn status(&mut self, line: &str) {
            self.push(format!("status: {line}"));
        }
        fn transition(&mut self, line: &str) {
            self.push(format!("transition: {line}"));
        }
        fn notice(&mut self, line: &str) {
            self.push(format!("notice: {line}"));
        }
        fn clear(&mut self) {
            self.push("clear".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::testing::RecordingSurface;
    use super::*;

    // Three ids as one batch generates them: the same timestamp head,
    // distinct random tails. The progress lines name a job by its tail.
    const A: &str = "019a1b2c-3d4e-7f00-8a11-0000aaaaaaaa";
    const B: &str = "019a1b2c-3d4e-7f00-8a11-0000bbbbbbbb";
    const OTHER: &str = "019a1b2c-3d4e-7f00-8a11-0000cccccccc";

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn event(channel: &str, value: Value) -> Event {
        Event {
            channel: channel.to_string(),
            value,
            lag: false,
        }
    }

    fn progress(job_id: &str, stage: &str, fraction: Option<f64>) -> Event {
        let mut value = json!({ "job_id": job_id, "kind": "book", "stage": stage });
        if let Some(f) = fraction {
            value["stage_progress"] = json!(f);
        }
        event("worker.progress", value)
    }

    fn finished(job_id: &str, state: &str, error: Option<&str>) -> Event {
        let mut lf = json!({
            "job_id": job_id,
            "kind": "book",
            "state": state,
            "finished_at": "2026-01-01T00:00:00Z",
        });
        if let Some(e) = error {
            lf["error"] = json!(e);
        }
        event(
            "queue.tick",
            json!({ "pending": 0, "running": 0, "last_finished": lf }),
        )
    }

    fn slot_of(state: &ViewState) -> Option<(String, String, Option<u8>)> {
        state
            .slot
            .as_ref()
            .map(|s| (s.job_id.clone(), s.stage.clone(), s.percent))
    }

    #[test]
    fn a_batch_moves_from_one_job_to_the_next_without_a_stale_stage() {
        let mut state = ViewState::for_jobs(&ids(&[A, B]));
        assert_eq!(state.status_line(), "0/2 waiting");

        let applied = state.apply(&progress(A, "processing", None));
        assert!(applied.stage_changed);
        assert_eq!(slot_of(&state), Some((A.into(), "processing".into(), None)));
        assert_eq!(state.status_line(), "0/2 aaaaaaaa [processing ----]");

        state.apply(&progress(A, "embed", Some(0.5)));
        assert_eq!(state.status_line(), "0/2 aaaaaaaa [embed  50%]");

        let applied = state.apply(&finished(A, "done", None));
        assert_eq!(applied.finished.map(|r| r.job_id), Some(A.to_string()));
        assert_eq!(applied.notice.as_deref(), Some("1/2 aaaaaaaa done"));
        assert_eq!(state.counts(), Some((1, 2)));
        assert_eq!(slot_of(&state), None, "finished job left its stage behind");
        assert_eq!(state.status_line(), "1/2 waiting");

        state.apply(&progress(B, "processing", None));
        assert_eq!(state.status_line(), "1/2 bbbbbbbb [processing ----]");
        assert!(!state.is_complete());
        state.apply(&finished(B, "done", None));
        assert!(state.is_complete());
    }

    #[test]
    fn another_batchs_jobs_neither_advance_the_count_nor_take_the_stage() {
        let mut state = ViewState::for_jobs(&ids(&[A]));

        let applied = state.apply(&finished(OTHER, "done", None));
        assert!(applied.finished.is_none());
        assert!(applied.notice.is_none());
        assert_eq!(
            state.counts(),
            Some((0, 1)),
            "a foreign job advanced the batch"
        );

        state.apply(&progress(OTHER, "processing", None));
        assert_eq!(slot_of(&state), None, "a foreign job took the stage slot");

        state.apply(&progress(A, "processing", None));
        state.apply(&progress(OTHER, "embed", Some(0.9)));
        assert_eq!(
            slot_of(&state),
            Some((A.into(), "processing".into(), None)),
            "a foreign job overwrote the batch job's stage"
        );
    }

    #[test]
    fn a_call_view_shows_its_method_and_no_queue_activity() {
        let mut state = ViewState::for_call("vectors.rebuild");
        state.apply(&progress(A, "processing", None));
        let applied = state.apply(&finished(A, "done", None));
        assert!(applied.finished.is_none());
        assert!(applied.notice.is_none());
        assert_eq!(slot_of(&state), None);
        assert_eq!(state.counts(), None);
        assert_eq!(state.status_line(), "vectors.rebuild");
        assert!(!state.is_complete());
    }

    #[test]
    fn a_repeated_job_id_counts_once() {
        let mut state = ViewState::for_jobs(&ids(&[A, A, B]));
        assert_eq!(state.counts(), Some((0, 2)));
        assert!(state.apply(&finished(A, "done", None)).finished.is_some());
        assert!(
            state.apply(&finished(A, "done", None)).finished.is_none(),
            "a second tick for a finished job was recorded again"
        );
        assert_eq!(state.counts(), Some((1, 2)));
    }

    #[test]
    fn malformed_progress_neither_panics_nor_invents_a_percentage() {
        let mut state = ViewState::for_jobs(&ids(&[A, B]));

        for fraction in [1.7, -0.2, f64::NAN] {
            state.apply(&progress(A, "embed", Some(fraction)));
            assert_eq!(slot_of(&state), Some((A.into(), "embed".into(), None)));
        }
        state.apply(&event(
            "worker.progress",
            json!({ "job_id": A, "kind": "book", "stage": "embed", "stage_progress": "0.5" }),
        ));
        assert_eq!(slot_of(&state).and_then(|s| s.2), None);

        // No stage: nothing to name, so the slot stays as it was.
        state.apply(&event("worker.progress", json!({ "job_id": B })));
        assert_eq!(slot_of(&state).map(|s| s.0), Some(A.to_string()));

        // An older daemon sends no kind; an unknown stage is shown as sent.
        state.apply(&event(
            "worker.progress",
            json!({ "job_id": B, "stage": "warming_up" }),
        ));
        assert_eq!(state.status_line(), "0/2 bbbbbbbb [warming_up ----]");

        // A tick without an outcome, and one with an unknown state.
        state.apply(&event("queue.tick", json!({ "pending": 1, "running": 1 })));
        state.apply(&finished(B, "exploded", None));
        assert_eq!(state.counts(), Some((0, 2)));
    }

    #[test]
    fn outcome_lines_appear_for_a_batch_and_carry_the_error() {
        let mut single = ViewState::for_jobs(&ids(&[A]));
        let applied = single.apply(&finished(A, "failed", Some("source unreadable")));
        assert!(applied.finished.is_some());
        assert!(
            applied.notice.is_none(),
            "a one-job wait repeats its summary line"
        );

        let mut batch = ViewState::for_jobs(&ids(&[A, B]));
        let applied = batch.apply(&finished(B, "failed", Some("source unreadable")));
        assert_eq!(
            applied.notice.as_deref(),
            Some("1/2 bbbbbbbb failed: source unreadable")
        );
    }

    #[test]
    fn a_lagged_stream_becomes_a_notice() {
        let mut state = ViewState::for_call("dryrun");
        let lag = Event {
            channel: "worker.progress".to_string(),
            value: Value::Null,
            lag: true,
        };
        assert_eq!(state.apply(&lag).notice.as_deref(), Some(LAG_NOTICE));
    }

    #[test]
    fn an_appending_surface_gets_one_line_per_stage_not_per_percent() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut view = ProgressView::with_surface(
            ViewState::for_jobs(&ids(&[A, B])),
            Box::new(RecordingSurface(Arc::clone(&log))),
        );
        view.apply(&progress(A, "embed", Some(0.1)));
        view.apply(&progress(A, "embed", Some(0.2)));
        view.apply(&progress(A, "embed", Some(0.3)));
        drop(view);
        let log = log.lock().unwrap();
        let transitions: Vec<&String> = log
            .iter()
            .filter(|e| e.starts_with("transition: "))
            .collect();
        assert_eq!(transitions, vec!["transition: 0/2 aaaaaaaa [embed]"]);
        assert_eq!(log.last().map(String::as_str), Some("clear"));
    }

    #[test]
    fn extract_finished_filters_by_pending_set() {
        let mut pending = HashSet::new();
        pending.insert("a".to_string());
        let ev = finished("a", "done", None).value;
        assert!(extract_finished(&ev, &pending).is_some());
        let other = finished("b", "done", None).value;
        assert!(extract_finished(&other, &pending).is_none());
    }

    #[test]
    fn extract_finished_recognises_skipped_duplicate_state() {
        // Sanity-guard the wire-token contract between the daemon and
        // the CLI wait path: a job whose ingest short-circuited on the
        // noop-if-up-to-date path must be extracted as a terminal
        // outcome, otherwise `bookrack ingest --wait` would silently
        // spin until the stall timeout when every job was a dedup skip.
        let mut pending = HashSet::new();
        pending.insert("a".to_string());
        let ev = finished("a", "skipped_duplicate", None).value;
        let record =
            extract_finished(&ev, &pending).expect("skipped_duplicate terminal recognised");
        assert!(matches!(record.state, JobOutcomeState::SkippedDuplicate));
    }

    #[test]
    fn extract_finished_recognises_needs_ocr_state() {
        // A scan source that ends in `needs_ocr` is a terminal the wait
        // path must stop on, otherwise `bookrack ingest` would spin
        // until the stall timeout when every job needed OCR.
        let mut pending = HashSet::new();
        pending.insert("a".to_string());
        let ev = finished("a", "needs_ocr", None).value;
        let record = extract_finished(&ev, &pending).expect("needs_ocr terminal recognised");
        assert!(matches!(record.state, JobOutcomeState::NeedsOcr));
    }

    fn ended(job_id: &str, state: JobOutcomeState, error: Option<&str>) -> JobOutcomeRecord {
        JobOutcomeRecord {
            job_id: job_id.to_string(),
            kind: "book".to_string(),
            state,
            error: error.map(String::from),
            finished_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn for_jobs_after_counts_ended_jobs_and_draws_their_outcomes() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let view = ProgressView::for_jobs_after(
            &[ended(A, JobOutcomeState::Failed, Some("boom"))],
            &ids(&[B]),
            Box::new(RecordingSurface(Arc::clone(&log))),
        );
        assert_eq!(view.pending(), 1);
        assert!(!view.is_complete());
        assert_eq!(
            *log.lock().unwrap(),
            ["status: 1/2 waiting", "notice: 1/2 aaaaaaaa failed: boom"]
        );

        let log = Arc::new(Mutex::new(Vec::new()));
        let view = ProgressView::for_jobs_after(
            &[ended(A, JobOutcomeState::Done, None)],
            &[],
            Box::new(RecordingSurface(Arc::clone(&log))),
        );
        assert!(view.is_complete());
        assert_eq!(*log.lock().unwrap(), ["status: 1/1 waiting"]);
    }
}
