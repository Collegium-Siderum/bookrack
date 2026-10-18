// SPDX-License-Identifier: Apache-2.0

//! `bookrack queue` — one-shot control-plane client for the
//! persistent ingest queue. Dispatches each [`QueueAction`] variant to
//! its matching JSON-RPC method:
//!
//! - `queue list`            → `queue.list`
//! - `queue pause`           → `queue.pause`
//! - `queue resume`          → `queue.resume`
//! - `queue clear`           → `queue.clear`
//! - `queue cancel <prefix>` → `ingest.cancel`
//! - `queue retry`           → `queue.retry`
//!
//! `queue list` renders a table in human mode (id8, kind, state,
//! queued (relative), priority, path basename) and the daemon payload
//! under `--json`. `--state` keeps the rows in one state and `--limit`
//! caps the rows, both applied to the `jobs` array in either mode;
//! the summary counts stay those of the whole queue. A `failed`
//! listing is grouped by cause, one header per distinct cause with
//! the rows beneath it. `--long` prints the full UUIDv7 in the id
//! column when the operator needs a copy-paste-able id. `queue retry`
//! fills in the default selector (`failed`) on the client, so the
//! params it sends always name one, and renders the ids the daemon
//! flipped. The pause / resume / clear / cancel actions remain thin
//! pass-throughs over the matching RPC.

use std::fmt::Write as _;
use std::path::PathBuf;

use bookrack_cli::render::ctx;
use bookrack_cli::render::human::{basename_or_dash, short_id};
use bookrack_cli::render::table::RowTable;
use bookrack_cli::render::time::relative_from_iso;
use bookrack_cli_grammar::{QueueAction, QueueStateFilter, RetryStateFilter};
use bookrack_core::queue::JobState;
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;

pub async fn run(action: QueueAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        QueueAction::List { long, state, limit } => {
            let filter = state.map(QueueStateFilter::job_state);
            let response =
                helpers::dispatch(&client, "queue.list", list_params(filter, limit)).await?;
            let listing = Listing::select(response, filter, limit);
            if ctx().is_json() {
                helpers::print_value(&listing.response);
                return Ok(());
            }
            if ctx().is_quiet() {
                return Ok(());
            }
            print!("{}", listing.render(long));
            Ok(())
        }
        QueueAction::Pause => helpers::call_and_print(&client, "queue.pause", Value::Null).await,
        QueueAction::Resume => helpers::call_and_print(&client, "queue.resume", Value::Null).await,
        QueueAction::Clear => helpers::call_and_print(&client, "queue.clear", Value::Null).await,
        QueueAction::Cancel { job_id } => {
            helpers::call_and_print(&client, "ingest.cancel", json!({ "job_id": job_id })).await
        }
        QueueAction::Retry { state, job } => {
            let state = state.unwrap_or(RetryStateFilter::Failed);
            let params = match job.as_deref() {
                Some(prefix) => json!({ "job_id": prefix }),
                None => json!({ "state": state.job_state().as_wire_str() }),
            };
            let response = helpers::dispatch(&client, "queue.retry", params).await?;
            if ctx().is_json() {
                helpers::print_value(&response);
                return Ok(());
            }
            if ctx().is_quiet() {
                return Ok(());
            }
            print!("{}", render_retry(&response, state, job.as_deref()));
            Ok(())
        }
    }
}

/// The human rendering of a `queue.retry` reply. A prefix retry names
/// the one id it flipped; a state retry counts the rows and lists
/// their ids beneath, or says there were none. A paused queue gets a
/// closing line, since the rows will not move until `queue resume`.
fn render_retry(response: &Value, state: RetryStateFilter, job: Option<&str>) -> String {
    let mut out = String::new();
    let retried: Vec<&str> = response
        .get("retried")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match (job, retried.as_slice()) {
        (Some(_), [id]) => {
            let _ = writeln!(out, "retried {id}");
        }
        (_, []) => {
            let _ = writeln!(out, "no {state} jobs to retry");
        }
        (_, ids) => {
            let noun = if ids.len() == 1 { "job" } else { "jobs" };
            let _ = writeln!(out, "retried {} {state} {noun}", ids.len());
            for id in ids {
                let _ = writeln!(out, "  {id}");
            }
        }
    }
    if response
        .get("paused")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let _ = writeln!(out, "queue is paused; run queue resume to start them");
    }
    out
}

/// The params `queue.list` is called with. The daemon's `limit` caps
/// the whole queue before any state is looked at, so it is sent only
/// when no state is selected; with a state the client fetches every
/// row, filters, and caps the matches itself.
fn list_params(filter: Option<JobState>, limit: Option<u32>) -> Value {
    match (filter, limit) {
        (None, Some(limit)) => json!({ "limit": limit }),
        _ => Value::Null,
    }
}

/// A `queue.list` reply reduced to the rows the operator asked for.
struct Listing {
    /// The daemon's reply with `jobs` replaced by the selected rows.
    /// Every other field is the daemon's.
    response: Value,
    /// The state the rows were kept by, if any.
    filter: Option<JobState>,
    /// Rows the selection matched before the cap: the filtered count
    /// under a filter, the queue's total otherwise.
    matched: usize,
}

impl Listing {
    fn select(mut response: Value, filter: Option<JobState>, limit: Option<u32>) -> Self {
        let Some(all) = response.get("jobs").and_then(Value::as_array).cloned() else {
            return Listing {
                response,
                filter,
                matched: 0,
            };
        };
        let (mut jobs, matched) = match filter {
            Some(state) => {
                let token = state.as_wire_str();
                let kept: Vec<Value> = all
                    .into_iter()
                    .filter(|job| job.get("state").and_then(Value::as_str) == Some(token))
                    .collect();
                let matched = kept.len();
                (kept, matched)
            }
            None => {
                let total = response
                    .pointer("/summary/total")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(all.len());
                (all, total)
            }
        };
        if let Some(limit) = limit {
            jobs.truncate(limit as usize);
        }
        response["jobs"] = Value::Array(jobs);
        Listing {
            response,
            filter,
            matched,
        }
    }

    fn jobs(&self) -> &[Value] {
        self.response
            .get("jobs")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The human rendering: the rows, a `showing N of M` line when the
    /// cap hid some, the whole-queue summary, and the paused notice.
    /// A single-state listing drops the state column; the `failed`
    /// listing is grouped by cause.
    fn render(&self, long: bool) -> String {
        let mut out = String::new();
        let jobs = self.jobs();
        match self.filter {
            _ if jobs.is_empty() => match self.filter {
                Some(state) => {
                    let _ = writeln!(out, "no jobs match state {}", state.as_wire_str());
                }
                None => {
                    let _ = writeln!(out, "queue is empty");
                }
            },
            Some(JobState::Failed) => {
                for group in failure_groups(jobs) {
                    let _ = writeln!(out, "{}  ({})", group.cause, group.rows.len());
                    let mut table = RowTable::new(["id", "kind", "queued", "priority", "path"]);
                    for job in group.rows {
                        table.push_row(row_cells(job, long, false));
                    }
                    let _ = writeln!(out, "{}", table.render());
                }
            }
            Some(_) => {
                let mut table = RowTable::new(["id", "kind", "queued", "priority", "path"]);
                for job in jobs {
                    table.push_row(row_cells(job, long, false));
                }
                let _ = writeln!(out, "{}", table.render());
            }
            None => {
                let mut table =
                    RowTable::new(["id", "kind", "state", "queued", "priority", "path"]);
                for job in jobs {
                    table.push_row(row_cells(job, long, true));
                }
                let _ = writeln!(out, "{}", table.render());
            }
        }
        if jobs.len() < self.matched {
            let _ = writeln!(out, "showing {} of {}", jobs.len(), self.matched);
        }
        if let Some(summary) = self.response.get("summary") {
            let _ = writeln!(out, "{}", render_queue_summary(summary));
        }
        if self
            .response
            .get("paused")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let _ = writeln!(out, "queue is paused");
        }
        out
    }
}

/// One table row for `job`; `with_state` adds the state cell after
/// the kind.
fn row_cells(job: &Value, long: bool, with_state: bool) -> Vec<String> {
    let id = job.get("id").and_then(Value::as_str).unwrap_or("");
    let id_cell = if long {
        id.to_string()
    } else {
        short_id(id).to_string()
    };
    let field = |key: &str| {
        job.get(key)
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string()
    };
    let queued = job
        .get("queued_at")
        .and_then(Value::as_str)
        .map(relative_from_iso)
        .unwrap_or_else(|| "-".to_string());
    let path = job
        .get("path")
        .and_then(Value::as_str)
        .map(|p| basename_or_dash(Some(p)).to_string())
        .unwrap_or_else(|| "-".to_string());
    let mut cells = vec![id_cell, field("kind")];
    if with_state {
        cells.push(field("state"));
    }
    cells.extend([queued, field("priority"), path]);
    cells
}

/// Failed rows that share a cause, in queue order.
struct FailureGroup<'a> {
    cause: String,
    rows: Vec<&'a Value>,
}

/// Groups failed rows by [`failure_cause`], largest group first; groups
/// of one size keep the order their first row has in the queue.
fn failure_groups(jobs: &[Value]) -> Vec<FailureGroup<'_>> {
    let mut groups: Vec<FailureGroup<'_>> = Vec::new();
    for job in jobs {
        let cause = failure_cause(job.get("error").and_then(Value::as_str));
        match groups.iter_mut().find(|group| group.cause == cause) {
            Some(group) => group.rows.push(job),
            None => groups.push(FailureGroup {
                cause,
                rows: vec![job],
            }),
        }
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.rows.len()));
    groups
}

/// The header a failed row without an error text is grouped under.
const NO_ERROR_RECORDED: &str = "(no error recorded)";

/// The cause a failed row is grouped under.
///
/// A job's `error` is a chain joined by `: `: the worker's label
/// (`ingest`), the registry's framing (`registry-mediated ingest`,
/// `registry-mediated glean`, `registry-mediated OCR intake`), then
/// the pipeline's own context and its cause. The label and the framing
/// are the same on every row and are dropped. Of the rest, the first
/// two segments are the key: one segment alone reads `extract error`
/// for both an unsupported format and an empty extraction, while the
/// full chain carries per-row detail that would give every row its
/// own group. A text that is nothing but framing is kept whole; a
/// missing or blank text groups under [`NO_ERROR_RECORDED`].
fn failure_cause(error: Option<&str>) -> String {
    let text = error.map(str::trim).unwrap_or_default();
    if text.is_empty() {
        return NO_ERROR_RECORDED.to_string();
    }
    let cause: Vec<&str> = text
        .split(": ")
        .skip_while(|segment| is_framing(segment))
        .take(2)
        .collect();
    if cause.is_empty() {
        text.to_string()
    } else {
        cause.join(": ")
    }
}

fn is_framing(segment: &str) -> bool {
    matches!(segment, "ingest" | "glean") || segment.starts_with("registry-mediated ")
}

/// Renders the queue summary counts as a single line, omitting states
/// with a zero count to keep the common case terse.
fn render_queue_summary(summary: &Value) -> String {
    let counts = [
        ("pending", "pending"),
        ("running", "running"),
        ("done", "done"),
        ("skipped_duplicate", "skipped"),
        ("needs_ocr", "needs-ocr"),
        ("failed", "failed"),
        ("cancelled", "cancelled"),
    ];
    let mut parts: Vec<String> = counts
        .iter()
        .filter_map(|(key, label)| {
            let n = summary.get(key).and_then(Value::as_u64).unwrap_or(0);
            (n > 0).then(|| format!("{label} {n}"))
        })
        .collect();
    let total = summary.get("total").and_then(Value::as_u64).unwrap_or(0);
    parts.push(format!("total {total}"));
    parts.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, state: &str, error: Option<&str>) -> Value {
        json!({
            "id": id,
            "kind": "book",
            "state": state,
            "queued_at": "2026-01-01T00:00:00Z",
            "priority": "normal",
            "path": format!("/synthetic/{id}.epub"),
            "error": error,
        })
    }

    const MISSING: &str = "ingest: registry-mediated ingest: reading the source file failed: No such file or directory (os error 2)";
    const NO_BODY: &str =
        "ingest: registry-mediated ingest: extract error: extraction produced no body blocks";

    /// Five rows in queue order; the failed rows sit third and fifth so
    /// a cap applied before the filter cannot reach the second one.
    fn reply() -> Value {
        json!({
            "schema_version": 6,
            "binary_schema_version": 6,
            "paused": true,
            "summary": { "pending": 1, "done": 1, "failed": 2, "cancelled": 1, "total": 5 },
            "jobs": [
                job("done-1", "done", None),
                job("cancelled-1", "cancelled", None),
                job("failed-1", "failed", Some(MISSING)),
                job("pending-1", "pending", None),
                job("failed-2", "failed", Some(NO_BODY)),
            ],
        })
    }

    fn ids(listing: &Listing) -> Vec<&str> {
        listing
            .jobs()
            .iter()
            .map(|job| job["id"].as_str().unwrap())
            .collect()
    }

    /// A cap travels to the daemon only when no state is selected: the
    /// daemon caps the whole queue, which under a filter would hide the
    /// matches past the cap.
    #[test]
    fn the_cap_reaches_the_daemon_only_without_a_filter() {
        assert_eq!(list_params(None, Some(20)), json!({ "limit": 20 }));
        assert_eq!(list_params(None, None), Value::Null);
        assert_eq!(list_params(Some(JobState::Failed), Some(20)), Value::Null);
        assert_eq!(list_params(Some(JobState::Failed), None), Value::Null);
    }

    #[test]
    fn the_filter_runs_before_the_cap() {
        let listing = Listing::select(reply(), Some(JobState::Failed), Some(1));
        assert_eq!(ids(&listing), ["failed-1"]);
        assert_eq!(listing.matched, 2, "the matches before the cap");

        let listing = Listing::select(reply(), Some(JobState::Failed), None);
        assert_eq!(ids(&listing), ["failed-1", "failed-2"]);
        assert_eq!(listing.matched, 2);
    }

    /// The selection rewrites `jobs` and nothing else: the summary keeps
    /// counting the whole queue and the envelope is the daemon's.
    #[test]
    fn the_selection_leaves_the_rest_of_the_reply_alone() {
        let original = reply();
        let listing = Listing::select(original.clone(), Some(JobState::Failed), Some(1));
        let mut expected = original.clone();
        expected["jobs"] = json!([job("failed-1", "failed", Some(MISSING))]);
        assert_eq!(listing.response, expected);
    }

    /// Without a filter the daemon has already applied the cap, so the
    /// match count is the queue's total rather than the rows received.
    #[test]
    fn an_unfiltered_listing_counts_the_queue_total_as_its_matches() {
        let mut capped_by_daemon = reply();
        capped_by_daemon["jobs"].as_array_mut().unwrap().truncate(2);
        let listing = Listing::select(capped_by_daemon, None, Some(2));
        assert_eq!(ids(&listing), ["done-1", "cancelled-1"]);
        assert_eq!(listing.matched, 5);
        assert!(listing.render(false).contains("showing 2 of 5"));
    }

    #[test]
    fn a_missing_jobs_array_is_left_as_it_came() {
        let reply = json!({ "paused": false });
        let listing = Listing::select(reply.clone(), Some(JobState::Failed), Some(1));
        assert_eq!(listing.response, reply);
        assert_eq!(listing.matched, 0);
    }

    /// The real error shapes the worker records: framing dropped,
    /// two segments kept, so the two extractor failures stay apart.
    #[test]
    fn the_cause_drops_the_framing_and_keeps_two_segments() {
        assert_eq!(
            failure_cause(Some(MISSING)),
            "reading the source file failed: No such file or directory (os error 2)"
        );
        assert_eq!(
            failure_cause(Some(NO_BODY)),
            "extract error: extraction produced no body blocks"
        );
        assert_eq!(
            failure_cause(Some(
                "ingest: registry-mediated ingest: extract error: unsupported format: mobi"
            )),
            "extract error: unsupported format"
        );
        assert_ne!(
            failure_cause(Some(NO_BODY)),
            failure_cause(Some(
                "ingest: registry-mediated ingest: extract error: unsupported format: mobi"
            )),
            "an empty extraction and an unsupported format are different causes"
        );
        assert_eq!(
            failure_cause(Some(
                "ingest: registry-mediated glean: I/O error: No such file or directory (os error 2)"
            )),
            "I/O error: No such file or directory (os error 2)"
        );
        assert_eq!(
            failure_cause(Some(
                "ingest: registry-mediated OCR intake: reading the source file failed: Is a directory (os error 21)"
            )),
            "reading the source file failed: Is a directory (os error 21)"
        );
        assert_eq!(
            failure_cause(Some(
                "ingest: registry-mediated ingest: vector store error: LanceDB error: lance error: Too many open files (os error 24)"
            )),
            "vector store error: LanceDB error"
        );
    }

    #[test]
    fn the_cause_keeps_a_text_without_framing_and_names_a_missing_one() {
        assert_eq!(failure_cause(Some("boom")), "boom");
        assert_eq!(
            failure_cause(Some("ingest: registry-mediated ingest")),
            "ingest: registry-mediated ingest"
        );
        assert_eq!(failure_cause(None), NO_ERROR_RECORDED);
        assert_eq!(failure_cause(Some("")), NO_ERROR_RECORDED);
        assert_eq!(failure_cause(Some("   ")), NO_ERROR_RECORDED);
    }

    /// The failed listing: one header per cause with the group's size,
    /// largest group first, rows beneath, no state column.
    #[test]
    fn the_failed_listing_groups_rows_under_cause_headers() {
        let mut reply = reply();
        reply["jobs"]
            .as_array_mut()
            .unwrap()
            .push(job("failed-3", "failed", Some(NO_BODY)));
        reply["jobs"]
            .as_array_mut()
            .unwrap()
            .push(job("failed-4", "failed", None));
        let out = Listing::select(reply, Some(JobState::Failed), None).render(false);
        let headers: Vec<&str> = out
            .lines()
            .filter(|line| line.ends_with(')') && line.contains("  ("))
            .collect();
        assert_eq!(
            headers,
            [
                "extract error: extraction produced no body blocks  (2)",
                "reading the source file failed: No such file or directory (os error 2)  (1)",
                "(no error recorded)  (1)",
            ],
            "{out}"
        );
        let no_body_header = out.find("no body blocks  (2)").unwrap();
        let missing_header = out.find("(os error 2)  (1)").unwrap();
        let failed_2 = out.find("failed-2").unwrap();
        let failed_3 = out.find("failed-3").unwrap();
        assert!(
            no_body_header < failed_2 && failed_2 < failed_3 && failed_3 < missing_header,
            "rows sit under their own header in queue order: {out}"
        );
        assert!(
            !out.lines()
                .any(|line| line.split_whitespace().any(|w| w == "state")),
            "a single-state listing has no state column: {out}"
        );
        assert!(!out.contains("showing "), "{out}");
        assert!(out.contains("failed 2  cancelled 1  total 5"), "{out}");
        assert!(out.ends_with("queue is paused\n"), "{out}");
    }

    #[test]
    fn a_capped_failed_listing_counts_the_rows_shown_and_the_matches() {
        let out = Listing::select(reply(), Some(JobState::Failed), Some(1)).render(false);
        assert!(out.contains("(os error 2)  (1)"), "{out}");
        assert!(!out.contains("failed-2"), "{out}");
        assert!(out.contains("showing 1 of 2\n"), "{out}");
    }

    #[test]
    fn an_empty_filtered_listing_names_the_filter() {
        let out = Listing::select(reply(), Some(JobState::Running), None).render(false);
        assert!(out.starts_with("no jobs match state running\n"), "{out}");
        assert!(!out.contains("queue is empty"), "{out}");
        assert!(!out.contains("showing "), "{out}");

        let mut empty = reply();
        empty["jobs"] = json!([]);
        empty["summary"] = json!({ "total": 0 });
        let out = Listing::select(empty, None, None).render(false);
        assert!(out.starts_with("queue is empty\n"), "{out}");
    }

    #[test]
    fn an_unfiltered_listing_keeps_the_state_column() {
        let out = Listing::select(reply(), None, None).render(false);
        let header = out.lines().nth(1).unwrap_or_default();
        assert!(header.contains("state"), "{out}");
        assert!(out.contains("cancelled-1"), "{out}");
        assert!(!out.contains("showing "), "{out}");
    }

    #[test]
    fn a_prefix_retry_names_the_one_id() {
        let reply = json!({ "ok": true, "retried": ["0d-full-id"], "paused": false });
        assert_eq!(
            render_retry(&reply, RetryStateFilter::Failed, Some("0d")),
            "retried 0d-full-id\n"
        );
    }

    /// A state retry counts the rows in the daemon's order and lists
    /// every id; one row takes the singular.
    #[test]
    fn a_state_retry_counts_and_lists_the_ids_in_reply_order() {
        let reply = json!({ "ok": true, "retried": ["second", "first"], "paused": false });
        assert_eq!(
            render_retry(&reply, RetryStateFilter::NeedsOcr, None),
            "retried 2 needs-ocr jobs\n  second\n  first\n"
        );
        let one = json!({ "ok": true, "retried": ["only"], "paused": false });
        assert_eq!(
            render_retry(&one, RetryStateFilter::Cancelled, None),
            "retried 1 cancelled job\n  only\n"
        );
    }

    #[test]
    fn an_empty_state_retry_names_the_state() {
        let reply = json!({ "ok": true, "retried": [], "paused": false });
        assert_eq!(
            render_retry(&reply, RetryStateFilter::Failed, None),
            "no failed jobs to retry\n"
        );
    }

    #[test]
    fn a_paused_queue_adds_the_resume_hint_once() {
        let paused = json!({ "ok": true, "retried": ["x"], "paused": true });
        let out = render_retry(&paused, RetryStateFilter::Failed, None);
        assert!(
            out.ends_with("queue is paused; run queue resume to start them\n"),
            "{out}"
        );
        assert_eq!(out.matches("queue is paused").count(), 1, "{out}");
        let running = json!({ "ok": true, "retried": ["x"], "paused": false });
        let out = render_retry(&running, RetryStateFilter::Failed, None);
        assert!(!out.contains("queue is paused"), "{out}");
    }

    #[test]
    fn a_single_state_listing_other_than_failed_is_a_plain_table() {
        let out = Listing::select(reply(), Some(JobState::Cancelled), None).render(false);
        assert!(out.contains("cancelled-1"), "{out}");
        assert!(!out.contains("  ("), "no cause headers: {out}");
        assert!(
            !out.lines()
                .any(|line| line.split_whitespace().any(|w| w == "state")),
            "{out}"
        );
    }
}
