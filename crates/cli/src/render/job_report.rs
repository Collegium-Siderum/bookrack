// SPDX-License-Identifier: Apache-2.0

//! Per-job outcome aggregation for async-job CLI commands.
//!
//! When a command enqueues one or more queue jobs and then waits for
//! them to finish, it folds the daemon's `queue.tick` events into a
//! [`JobOutcomeReport`]. The report exposes both a one-line human
//! summary (used by `ingest`, `papers ingest`, ...) and a serializable
//! shape for the `--json` rendering mode.

use std::time::Duration;

use bookrack_core::Problem;
use serde::Serialize;

use super::human::short_id;

/// Terminal state observed for one job.
///
/// `SkippedDuplicate` covers a job whose source was already in the
/// catalog with up-to-date stamps, so the worker short-circuited
/// without writing. `NeedsOcr` covers a scan source with no usable text
/// layer, registered as a `needs_ocr` anchor for a later OCR pass. Both
/// are success terminals — distinct from `Done` so batch summaries can
/// reconcile against `library.stats` and steer the operator to the OCR
/// worklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcomeState {
    Done,
    SkippedDuplicate,
    NeedsOcr,
    Failed,
    Cancelled,
}

impl JobOutcomeState {
    pub fn from_wire(state: &str) -> Option<Self> {
        match state {
            "done" | "Done" => Some(Self::Done),
            "skipped_duplicate" | "SkippedDuplicate" => Some(Self::SkippedDuplicate),
            "needs_ocr" | "NeedsOcr" => Some(Self::NeedsOcr),
            "failed" | "Failed" => Some(Self::Failed),
            "cancelled" | "Cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// True for terminal states the worker treats as success: the
    /// pipeline reported `Ok` on the runner future. Callers use this
    /// to decide whether a batch overall succeeded or not.
    pub fn is_success(self) -> bool {
        matches!(self, Self::Done | Self::SkippedDuplicate | Self::NeedsOcr)
    }
}

/// Per-job record extracted from `QueueTick::last_finished`.
#[derive(Debug, Clone, Serialize)]
pub struct JobOutcomeRecord {
    pub job_id: String,
    /// Pipeline label: `"book"` for ingest, `"paper"` for glean.
    pub kind: String,
    pub state: JobOutcomeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The classified diagnostic the daemon recorded beside the failure,
    /// when it recorded one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<Problem>,
    pub finished_at: String,
}

impl JobOutcomeRecord {
    /// The one-line reason for the outcome: the recorded diagnostic's
    /// summary when the daemon classified the failure, else the
    /// flattened error text.
    pub fn reason(&self) -> Option<&str> {
        self.problem
            .as_ref()
            .map(|p| p.summary.as_str())
            .or(self.error.as_deref())
    }
}

/// Aggregate count of terminal states across the report.
#[derive(Debug, Clone, Default, Serialize)]
pub struct JobTotals {
    pub done: u32,
    pub skipped_duplicate: u32,
    pub needs_ocr: u32,
    pub failed: u32,
    pub cancelled: u32,
}

impl JobTotals {
    fn record(&mut self, state: JobOutcomeState) {
        match state {
            JobOutcomeState::Done => self.done += 1,
            JobOutcomeState::SkippedDuplicate => self.skipped_duplicate += 1,
            JobOutcomeState::NeedsOcr => self.needs_ocr += 1,
            JobOutcomeState::Failed => self.failed += 1,
            JobOutcomeState::Cancelled => self.cancelled += 1,
        }
    }
}

/// Final report for one batch of awaited jobs.
///
/// A wait that stops before every job has ended still produces a
/// report: `jobs` holds the records collected so far and `unfinished`
/// names the jobs it did not see end, in the order they were selected.
/// A complete report has an empty `unfinished`, and the field is then
/// left out of the serialized shape.
#[derive(Debug, Clone, Serialize)]
pub struct JobOutcomeReport {
    pub jobs: Vec<JobOutcomeRecord>,
    pub elapsed_secs: f64,
    pub totals: JobTotals,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unfinished: Vec<String>,
}

impl JobOutcomeReport {
    pub fn new(jobs: Vec<JobOutcomeRecord>, elapsed: Duration) -> Self {
        let mut totals = JobTotals::default();
        for j in &jobs {
            totals.record(j.state);
        }
        Self {
            jobs,
            elapsed_secs: elapsed.as_secs_f64(),
            totals,
            unfinished: Vec::new(),
        }
    }

    /// The same report, naming the jobs the wait did not see end.
    pub fn with_unfinished(mut self, ids: Vec<String>) -> Self {
        self.unfinished = ids;
        self
    }

    /// True when the wait saw every job it covered end.
    pub fn is_complete(&self) -> bool {
        self.unfinished.is_empty()
    }

    /// True when every awaited job reached a terminal success state
    /// (`Done`, `SkippedDuplicate`, or `NeedsOcr`) — i.e. none failed or
    /// were cancelled, and none is still unfinished.
    pub fn all_succeeded(&self) -> bool {
        self.is_complete() && self.totals.failed == 0 && self.totals.cancelled == 0
    }

    /// One `hint: <hint>` line per distinct hint among the records'
    /// diagnostics, in record order; a hint several jobs share is
    /// printed once.
    pub fn hint_lines(&self) -> Vec<String> {
        let mut seen: Vec<&str> = Vec::new();
        for hint in self
            .jobs
            .iter()
            .filter_map(|job| job.problem.as_ref()?.data.hint.as_deref())
        {
            if !seen.contains(&hint) {
                seen.push(hint);
            }
        }
        seen.into_iter()
            .map(|hint| format!("hint: {hint}"))
            .collect()
    }

    /// One-line human summary for a single-job report.
    ///
    /// `action` is the verb stem (e.g. `"Ingested"`). `label` is the
    /// noun the operator can recognise (typically a basename). A
    /// single job's state word carries the job's reason after a colon
    /// when the outcome recorded one ([`JobOutcomeRecord::reason`]). The report degrades gracefully
    /// when more than one job is present (falls back to the aggregate
    /// counts). An incomplete report counts the ended jobs against the
    /// whole batch and says how many are still queued.
    pub fn format_one_line(&self, action: &str, label: &str) -> String {
        let elapsed = format_elapsed(self.elapsed_secs);
        if !self.is_complete() {
            let total = self.jobs.len() + self.unfinished.len();
            let mut parts = self.count_parts();
            parts.push(format!("{} still queued", self.unfinished.len()));
            return format!(
                "{action} {done} of {total} {label} in {elapsed} ({summary})",
                done = self.jobs.len(),
                summary = parts.join(", "),
            );
        }
        match self.jobs.as_slice() {
            // A one-job batch prints no per-job outcome line, so the
            // error travels on the summary itself.
            [only] => {
                let mut verb = state_word(only.state).to_string();
                if let Some(reason) = only.reason() {
                    verb.push_str(": ");
                    verb.push_str(reason);
                }
                format!(
                    "{action} {label} as {id} in {elapsed} ({verb})",
                    id = short_id(&only.job_id),
                )
            }
            many => format!(
                "{action} {n} {label} in {elapsed} ({summary})",
                n = many.len(),
                summary = self.count_parts().join(", "),
            ),
        }
    }

    /// The non-zero totals as `"<n> <state word>"` fragments, in the
    /// order the states are listed.
    fn count_parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.totals.done > 0 {
            parts.push(format!("{} done", self.totals.done));
        }
        if self.totals.skipped_duplicate > 0 {
            parts.push(format!(
                "{} already in catalog",
                self.totals.skipped_duplicate
            ));
        }
        if self.totals.needs_ocr > 0 {
            parts.push(format!("{} need OCR", self.totals.needs_ocr));
        }
        if self.totals.failed > 0 {
            parts.push(format!("{} failed", self.totals.failed));
        }
        if self.totals.cancelled > 0 {
            parts.push(format!("{} cancelled", self.totals.cancelled));
        }
        parts
    }
}

/// The word a human summary uses for `state`.
pub fn state_word(state: JobOutcomeState) -> &'static str {
    match state {
        JobOutcomeState::Done => "done",
        JobOutcomeState::SkippedDuplicate => "already in catalog",
        JobOutcomeState::NeedsOcr => "needs OCR",
        JobOutcomeState::Failed => "failed",
        JobOutcomeState::Cancelled => "cancelled",
    }
}

fn format_elapsed(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 60.0 {
        format!("{:.1}s", secs)
    } else {
        let mins = (secs / 60.0).floor() as u64;
        let rem = secs - (mins as f64) * 60.0;
        format!("{mins}m{rem:.0}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, state: JobOutcomeState) -> JobOutcomeRecord {
        JobOutcomeRecord {
            job_id: id.to_string(),
            kind: "book".to_string(),
            state,
            error: None,
            problem: None,
            finished_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    /// A failed record carrying both the flattened text and the
    /// classified diagnostic.
    fn failed_with_problem(
        id: &str,
        error: &str,
        summary: &str,
        hint: Option<&str>,
    ) -> JobOutcomeRecord {
        let mut record = rec(id, JobOutcomeState::Failed);
        record.error = Some(error.to_string());
        let mut problem = Problem::new(summary);
        if let Some(hint) = hint {
            problem = problem.hint(hint);
        }
        record.problem = Some(problem);
        record
    }

    /// The one-job summary names the recorded summary when there is
    /// one, and falls back to the flattened text when there is not.
    #[test]
    fn a_one_job_summary_prefers_the_recorded_summary() {
        let r = JobOutcomeReport::new(
            vec![failed_with_problem(
                "0190f6c0-ac42-7e05-7000-deadbeef",
                "ingest: registry-mediated ingest: embed error: no model",
                "cannot embed: the model is not available",
                Some("Pull it first."),
            )],
            Duration::from_millis(3_200),
        );
        assert_eq!(
            r.format_one_line("Ingested", "sample.epub"),
            "Ingested sample.epub as deadbeef in 3.2s (failed: cannot embed: the model is not available)"
        );
        assert_eq!(r.hint_lines(), ["hint: Pull it first."]);

        let mut plain = rec("0190f6c0-ac42-7e05-7000-deadbeef", JobOutcomeState::Failed);
        plain.error = Some("source unreadable".to_string());
        let r = JobOutcomeReport::new(vec![plain], Duration::from_millis(3_200));
        assert!(
            r.format_one_line("Ingested", "sample.epub")
                .ends_with("(failed: source unreadable)")
        );
        assert!(r.hint_lines().is_empty());
    }

    /// Three failures, two distinct hints, one record without any:
    /// each hint once, in record order.
    #[test]
    fn hint_lines_prints_each_distinct_hint_once() {
        let r = JobOutcomeReport::new(
            vec![
                failed_with_problem("a", "e", "cannot embed", Some("Pull it first.")),
                failed_with_problem("b", "e", "cannot read", None),
                failed_with_problem("c", "e", "could not reach Ollama", Some("Start Ollama.")),
                failed_with_problem("d", "e", "cannot embed", Some("Pull it first.")),
            ],
            Duration::from_secs(1),
        );
        assert_eq!(
            r.hint_lines(),
            ["hint: Pull it first.", "hint: Start Ollama."]
        );
    }

    #[test]
    fn totals_aggregate_states() {
        let r = JobOutcomeReport::new(
            vec![
                rec("a", JobOutcomeState::Done),
                rec("b", JobOutcomeState::Failed),
                rec("c", JobOutcomeState::Done),
            ],
            Duration::from_secs(3),
        );
        assert_eq!(r.totals.done, 2);
        assert_eq!(r.totals.failed, 1);
        assert!(!r.all_succeeded());
    }

    #[test]
    fn needs_ocr_is_a_success_terminal_and_summarises() {
        let r = JobOutcomeReport::new(
            vec![
                rec("a", JobOutcomeState::Done),
                rec("b", JobOutcomeState::NeedsOcr),
                rec("c", JobOutcomeState::NeedsOcr),
            ],
            Duration::from_millis(500),
        );
        assert_eq!(r.totals.needs_ocr, 2);
        // A batch of scan sources is not a failure.
        assert!(r.all_succeeded());
        assert!(JobOutcomeState::NeedsOcr.is_success());
        let line = r.format_one_line("Ingested", "books");
        assert!(line.contains("1 done"));
        assert!(line.contains("2 need OCR"), "got: {line}");
    }

    #[test]
    fn from_wire_accepts_needs_ocr() {
        assert_eq!(
            JobOutcomeState::from_wire("needs_ocr"),
            Some(JobOutcomeState::NeedsOcr)
        );
        assert_eq!(
            JobOutcomeState::from_wire("NeedsOcr"),
            Some(JobOutcomeState::NeedsOcr)
        );
    }

    #[test]
    fn one_line_uses_short_id_for_single_job() {
        let r = JobOutcomeReport::new(
            vec![rec(
                "0190f6c0-ac42-7e05-7000-deadbeef",
                JobOutcomeState::Done,
            )],
            Duration::from_millis(12_400),
        );
        let line = r.format_one_line("Ingested", "sample.epub");
        assert!(line.contains("Ingested sample.epub as deadbeef"));
        assert!(line.contains("12.4s"));
        assert!(line.contains("done"));
    }

    #[test]
    fn one_line_names_the_error_of_a_single_failed_job() {
        let mut failed = rec("0190f6c0-ac42-7e05-7000-deadbeef", JobOutcomeState::Failed);
        failed.error = Some("source unreadable".to_string());
        let r = JobOutcomeReport::new(vec![failed], Duration::from_millis(3_200));
        assert_eq!(
            r.format_one_line("Ingested", "sample.epub"),
            "Ingested sample.epub as deadbeef in 3.2s (failed: source unreadable)"
        );

        // A terminal that carries no error keeps the bare state word.
        let r = JobOutcomeReport::new(
            vec![rec(
                "0190f6c0-ac42-7e05-7000-deadbeef",
                JobOutcomeState::Cancelled,
            )],
            Duration::from_millis(3_200),
        );
        assert_eq!(
            r.format_one_line("Ingested", "sample.epub"),
            "Ingested sample.epub as deadbeef in 3.2s (cancelled)"
        );
    }

    #[test]
    fn one_line_summarises_multiple_jobs() {
        let r = JobOutcomeReport::new(
            vec![
                rec("a", JobOutcomeState::Done),
                rec("b", JobOutcomeState::Failed),
            ],
            Duration::from_millis(500),
        );
        let line = r.format_one_line("Ingested", "books");
        assert!(line.contains("Ingested 2 books"));
        assert!(line.contains("500ms"));
        assert!(line.contains("1 done"));
        assert!(line.contains("1 failed"));
    }

    #[test]
    fn an_incomplete_report_counts_the_ended_against_the_batch() {
        let r = JobOutcomeReport::new(
            vec![
                rec("a", JobOutcomeState::Done),
                rec("b", JobOutcomeState::Failed),
            ],
            Duration::from_millis(500),
        )
        .with_unfinished(vec!["c".into(), "d".into()]);
        assert!(!r.is_complete());
        assert_eq!(
            r.format_one_line("Ingested", "books"),
            "Ingested 2 of 4 books in 500ms (1 done, 1 failed, 2 still queued)"
        );

        let r = JobOutcomeReport::new(Vec::new(), Duration::from_millis(500))
            .with_unfinished(vec!["c".into()]);
        assert_eq!(
            r.format_one_line("Ingested", "sample.epub"),
            "Ingested 0 of 1 sample.epub in 500ms (1 still queued)"
        );
    }

    #[test]
    fn an_incomplete_report_without_failures_has_not_succeeded() {
        let r = JobOutcomeReport::new(vec![rec("a", JobOutcomeState::Done)], Duration::ZERO)
            .with_unfinished(vec!["b".into()]);
        assert!(!r.all_succeeded());
        assert!(r.clone().with_unfinished(Vec::new()).all_succeeded());
    }

    #[test]
    fn unfinished_is_serialized_only_when_present() {
        let complete = JobOutcomeReport::new(vec![rec("a", JobOutcomeState::Done)], Duration::ZERO);
        let json = serde_json::to_value(&complete).unwrap();
        assert!(json.get("unfinished").is_none(), "{json}");

        let partial = complete.with_unfinished(vec!["b".into()]);
        let json = serde_json::to_value(&partial).unwrap();
        assert_eq!(json["unfinished"], serde_json::json!(["b"]));
    }

    #[test]
    fn from_wire_accepts_camel_and_lower() {
        assert_eq!(
            JobOutcomeState::from_wire("done"),
            Some(JobOutcomeState::Done)
        );
        assert_eq!(
            JobOutcomeState::from_wire("Done"),
            Some(JobOutcomeState::Done)
        );
        assert_eq!(
            JobOutcomeState::from_wire("Cancelled"),
            Some(JobOutcomeState::Cancelled)
        );
        assert_eq!(
            JobOutcomeState::from_wire("skipped_duplicate"),
            Some(JobOutcomeState::SkippedDuplicate)
        );
        assert_eq!(JobOutcomeState::from_wire("pending"), None);
    }

    #[test]
    fn totals_track_skipped_duplicate_and_report_still_succeeds() {
        let r = JobOutcomeReport::new(
            vec![
                rec("a", JobOutcomeState::Done),
                rec("b", JobOutcomeState::SkippedDuplicate),
                rec("c", JobOutcomeState::SkippedDuplicate),
            ],
            Duration::from_secs(1),
        );
        assert_eq!(r.totals.done, 1);
        assert_eq!(r.totals.skipped_duplicate, 2);
        assert!(r.all_succeeded());
    }

    #[test]
    fn one_line_summarises_skipped_duplicate_batch() {
        let r = JobOutcomeReport::new(
            vec![
                rec("a", JobOutcomeState::Done),
                rec("b", JobOutcomeState::SkippedDuplicate),
            ],
            Duration::from_millis(200),
        );
        let line = r.format_one_line("Ingested", "books");
        assert!(line.contains("1 done"));
        assert!(line.contains("1 already in catalog"));
    }
}
