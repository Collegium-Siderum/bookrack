// SPDX-License-Identifier: Apache-2.0

//! Per-job outcome aggregation for async-job CLI commands.
//!
//! When a command enqueues one or more queue jobs and then waits for
//! them to finish, it folds the daemon's `queue.tick` events into a
//! [`JobOutcomeReport`]. The report exposes both a one-line human
//! summary (used by `ingest`, `papers ingest`, ...) and a serializable
//! shape for the `--json` rendering mode.

use std::time::Duration;

use serde::Serialize;

use super::human::short_id;

/// Terminal state observed for one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobOutcomeState {
    Done,
    Failed,
    Cancelled,
}

impl JobOutcomeState {
    pub fn from_wire(state: &str) -> Option<Self> {
        match state {
            "done" | "Done" => Some(Self::Done),
            "failed" | "Failed" => Some(Self::Failed),
            "cancelled" | "Cancelled" => Some(Self::Cancelled),
            _ => None,
        }
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
    pub finished_at: String,
}

/// Aggregate count of terminal states across the report.
#[derive(Debug, Clone, Default, Serialize)]
pub struct JobTotals {
    pub done: u32,
    pub failed: u32,
    pub cancelled: u32,
}

impl JobTotals {
    fn record(&mut self, state: JobOutcomeState) {
        match state {
            JobOutcomeState::Done => self.done += 1,
            JobOutcomeState::Failed => self.failed += 1,
            JobOutcomeState::Cancelled => self.cancelled += 1,
        }
    }
}

/// Final report for one batch of awaited jobs.
#[derive(Debug, Clone, Serialize)]
pub struct JobOutcomeReport {
    pub jobs: Vec<JobOutcomeRecord>,
    pub elapsed_secs: f64,
    pub totals: JobTotals,
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
        }
    }

    /// True when every awaited job reached `Done`.
    pub fn all_succeeded(&self) -> bool {
        self.totals.failed == 0 && self.totals.cancelled == 0
    }

    /// One-line human summary for a single-job report.
    ///
    /// `action` is the verb stem (e.g. `"Ingested"`). `label` is the
    /// noun the operator can recognise (typically a basename). The
    /// report degrades gracefully when more than one job is present
    /// (falls back to the aggregate counts).
    pub fn format_one_line(&self, action: &str, label: &str) -> String {
        let elapsed = format_elapsed(self.elapsed_secs);
        match self.jobs.as_slice() {
            [only] => format!(
                "{action} {label} as {id} in {elapsed} ({verb})",
                id = short_id(&only.job_id),
                verb = state_word(only.state),
            ),
            many => {
                let mut parts = Vec::new();
                if self.totals.done > 0 {
                    parts.push(format!("{} done", self.totals.done));
                }
                if self.totals.failed > 0 {
                    parts.push(format!("{} failed", self.totals.failed));
                }
                if self.totals.cancelled > 0 {
                    parts.push(format!("{} cancelled", self.totals.cancelled));
                }
                format!(
                    "{action} {n} {label} in {elapsed} ({summary})",
                    n = many.len(),
                    summary = parts.join(", "),
                )
            }
        }
    }
}

fn state_word(state: JobOutcomeState) -> &'static str {
    match state {
        JobOutcomeState::Done => "done",
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
            finished_at: "2026-01-01T00:00:00Z".to_string(),
        }
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
    fn one_line_uses_short_id_for_single_job() {
        let r = JobOutcomeReport::new(
            vec![rec(
                "0190f6c0-ac42-7e05-7000-deadbeef",
                JobOutcomeState::Done,
            )],
            Duration::from_millis(12_400),
        );
        let line = r.format_one_line("Ingested", "sample.epub");
        assert!(line.contains("Ingested sample.epub as 0190f6c0"));
        assert!(line.contains("12.4s"));
        assert!(line.contains("done"));
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
        assert_eq!(JobOutcomeState::from_wire("pending"), None);
    }
}
