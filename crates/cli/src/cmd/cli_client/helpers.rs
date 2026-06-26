//! Shared connection, rendering, and progress plumbing for the
//! one-shot CLI clients in this module tree.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default per-RPC timeout applied to every [`ControlClient`] the CLI
/// builds through [`connect`]. Sized generously so steady-state ops
/// never trip it on a healthy daemon while still catching a daemon
/// that has wedged. Adjust through the matching env knob in the next
/// pass.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Default stall timeout for [`await_jobs`]: how long the wait loop
/// will tolerate zero events before reporting that the daemon has
/// stopped progressing. The timer resets on every event seen, so
/// long-running jobs that keep emitting `worker.progress` survive
/// regardless of total elapsed time.
const DEFAULT_AWAIT_STALL_TIMEOUT: Duration = Duration::from_secs(60);

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::ctx;
use bookrack_cli::render::job_report::{JobOutcomeRecord, JobOutcomeReport, JobOutcomeState};
use bookrack_control_client::{ControlClient, ControlError, Event};
use eyre::{Context, Result};
use serde_json::Value;
use tokio::sync::broadcast;

/// Discover the daemon and open a control-plane connection. Returns
/// [`BookrackCliError::DaemonNotRunning`] when no daemon is listening
/// and [`BookrackCliError::DaemonUnreachable`] for every other
/// transport failure, so the top-level reporter in `main` can render
/// a uniform "bookrack: …" prefix and map to the right exit code
/// instead of every call site re-rolling its own `eprintln!`.
///
/// Sets a default per-RPC timeout of [`DEFAULT_CALL_TIMEOUT`] on the
/// returned client so a hung daemon surfaces as
/// [`ControlError::Timeout`] instead of an unkillable foreground.
pub async fn connect(runtime_dir: Option<&Path>) -> Result<Arc<ControlClient>> {
    let socket = match bookrack_control_client::discover(runtime_dir) {
        Ok(socket) => socket,
        Err(ControlError::NotRunning) => return Err(BookrackCliError::DaemonNotRunning.into()),
        Err(source) => return Err(BookrackCliError::DaemonUnreachable { source }.into()),
    };
    match bookrack_control_client::connect_with_default_timeout(&socket, DEFAULT_CALL_TIMEOUT).await
    {
        Ok(client) => Ok(Arc::new(client)),
        Err(ControlError::NotRunning) => Err(BookrackCliError::DaemonNotRunning.into()),
        Err(source) => Err(BookrackCliError::DaemonUnreachable { source }.into()),
    }
}

/// Send one JSON-RPC request and return the `result` payload.
///
/// Pure RPC: no event subscription, no rendering, no printing. The
/// building block underneath every other call helper in this module
/// and the unit of work subcommands compose with `await_jobs` when
/// they want to wait for queue completion.
pub async fn dispatch(client: &ControlClient, method: &str, params: Value) -> Result<Value> {
    client
        .call_raw(method, params)
        .await
        .with_context(|| format!("{method} rpc"))
}

/// Call the named RPC, await the response, and pretty-print the
/// `result` on stdout.
pub async fn call_and_print(client: &ControlClient, method: &str, params: Value) -> Result<()> {
    let value = dispatch(client, method, params).await?;
    print_value(&value);
    Ok(())
}

/// Run a long-lived command: subscribe to the broadcast, kick off
/// the call concurrently, render every event that arrives while the
/// call is in flight, then print the final response.
pub async fn call_with_progress(
    client: Arc<ControlClient>,
    method: &str,
    params: Value,
) -> Result<()> {
    let value = call_with_progress_value(client, method, params).await?;
    print_value(&value);
    Ok(())
}

/// Variant of [`call_with_progress`] that returns the RPC result
/// instead of printing it. Callers that want to render a structured
/// response themselves use this.
pub async fn call_with_progress_value(
    client: Arc<ControlClient>,
    method: &str,
    params: Value,
) -> Result<Value> {
    let mut events = client
        .subscribe()
        .await
        .context("subscribe to control-plane events")?;
    let method_owned = method.to_string();
    let client_for_call = Arc::clone(&client);
    let call_future = async move {
        client_for_call
            .call_raw(&method_owned, params)
            .await
            .map_err(eyre::Report::from)
    };
    tokio::pin!(call_future);
    let value = loop {
        tokio::select! {
            biased;
            res = &mut call_future => break res?,
            ev = events.recv() => match ev {
                Ok(event) => render_event(&event),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    break (&mut call_future).await?;
                }
            },
        }
    };
    finish_progress_line();
    Ok(value)
}

/// Wait for every job in `job_ids` to reach a terminal queue state
/// (`Done`, `Failed`, or `Cancelled`) and return the aggregated
/// [`JobOutcomeReport`].
///
/// The caller passes in a `broadcast::Receiver` obtained from
/// [`ControlClient::subscribe`] **before** the request that produced
/// the job ids was issued. Subscribing first avoids the race where
/// a `queue.tick` carrying `last_finished` fires between the RPC
/// returning and the wait loop starting.
///
/// `worker.progress` events are still rendered while the wait is in
/// flight, so the operator sees per-stage progress on stderr.
///
/// Bounded by a stall timeout equal to [`DEFAULT_AWAIT_STALL_TIMEOUT`]
/// so a daemon that silently stops emitting events does not leave the
/// CLI hanging forever; the timer resets on every event seen.
pub async fn await_jobs(
    rx: broadcast::Receiver<Event>,
    job_ids: &[String],
) -> Result<JobOutcomeReport> {
    let report = await_jobs_from_rx(
        rx,
        job_ids.to_vec(),
        Instant::now(),
        DEFAULT_AWAIT_STALL_TIMEOUT,
    )
    .await?;
    finish_progress_line();
    Ok(report)
}

/// Test-friendly core of [`await_jobs`]: drains the receiver until
/// every awaited id has appeared in a `queue.tick`'s `last_finished`.
///
/// `stall_timeout` bounds the wait between consecutive events. The
/// timer resets every time an event lands; a stretch with no events
/// at all surfaces as an error instead of hanging the CLI.
async fn await_jobs_from_rx(
    mut rx: broadcast::Receiver<Event>,
    job_ids: Vec<String>,
    started_at: Instant,
    stall_timeout: Duration,
) -> Result<JobOutcomeReport> {
    if job_ids.is_empty() {
        return Ok(JobOutcomeReport::new(Vec::new(), started_at.elapsed()));
    }
    let mut pending: HashSet<String> = job_ids.into_iter().collect();
    let mut jobs: Vec<JobOutcomeRecord> = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        match tokio::time::timeout(stall_timeout, rx.recv()).await {
            Err(_elapsed) => {
                return Err(eyre::eyre!(
                    "control event stream stalled for {}s with {} job(s) still pending; \
                     daemon may be unresponsive",
                    stall_timeout.as_secs(),
                    pending.len()
                ));
            }
            Ok(Ok(event)) => {
                if event.lag {
                    eprintln!("\nbookrack: event stream lagged; waiting on remaining jobs");
                    continue;
                }
                render_event(&event);
                if event.channel == "queue.tick"
                    && let Some(record) = extract_finished(&event.value, &pending)
                {
                    pending.remove(&record.job_id);
                    jobs.push(record);
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err(eyre::eyre!(
                    "control event stream closed before {} job(s) finished",
                    pending.len()
                ));
            }
        }
    }
    Ok(JobOutcomeReport::new(jobs, started_at.elapsed()))
}

/// Parses a `last_finished` payload off a `queue.tick` value into a
/// [`JobOutcomeRecord`]. Returns `None` when the tick has no terminal
/// outcome, when the id is not one of the awaited jobs, or when any
/// required field is missing or unknown.
fn extract_finished(value: &Value, pending: &HashSet<String>) -> Option<JobOutcomeRecord> {
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

/// Render one broadcast [`Event`] to stderr.
///
/// No-op in `Json` and `Quiet` render modes so machine-output and
/// silent-success paths stay clean. `worker.progress` rewrites the
/// current row with `\r`; `queue.tick` reuses the same row to show
/// pending / running counts; other channels are dropped.
pub fn render_event(event: &Event) {
    let ctx = ctx();
    if ctx.is_json() || ctx.is_quiet() {
        return;
    }
    if event.lag {
        eprintln!("\nbookrack: event stream lagged; progress may be incomplete");
        return;
    }
    match event.channel.as_str() {
        "worker.progress" => render_worker_progress(&event.value),
        "queue.tick" => render_queue_tick(&event.value),
        _ => {}
    }
}

fn render_worker_progress(value: &Value) {
    let job = value.get("job_id").and_then(Value::as_str).unwrap_or("?");
    let stage = value.get("stage").and_then(Value::as_str).unwrap_or("?");
    let progress = value
        .get("stage_progress")
        .and_then(Value::as_f64)
        .map(|p| format!(" {:>3.0}%", p * 100.0))
        .unwrap_or_default();
    let message = value.get("message").and_then(Value::as_str).unwrap_or("");
    let job_short: String = job.chars().take(8).collect();
    eprint!("\r{job_short} [{stage}{progress}] {message}");
    std::io::stderr().flush().ok();
}

fn render_queue_tick(value: &Value) {
    let pending = value.get("pending").and_then(Value::as_u64).unwrap_or(0);
    let running = value.get("running").and_then(Value::as_u64).unwrap_or(0);
    let current = value
        .get("current")
        .and_then(Value::as_str)
        .map(|c| c.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "-".to_string());
    eprint!("\r[QUEUE] current={current} pending={pending} running={running}");
    std::io::stderr().flush().ok();
}

/// Emit a trailing newline after the progress row so the final
/// stdout payload starts on a fresh line.
pub fn finish_progress_line() {
    if ctx().is_json() || ctx().is_quiet() {
        return;
    }
    eprintln!();
}

/// Extract `job_ids` (an array of strings) or `job_id` (a single
/// string) from an enqueue-style RPC response, returning the empty
/// vector when neither shape is present.
pub fn extract_job_ids(value: &Value) -> Vec<String> {
    if let Some(arr) = value.get("job_ids").and_then(Value::as_array) {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    if let Some(s) = value.get("job_id").and_then(Value::as_str) {
        return vec![s.to_string()];
    }
    Vec::new()
}

/// Print a one-shot summary for a finished batch of async jobs.
///
/// Mode-aware: silent in `Quiet`; a pretty-printed
/// [`JobOutcomeReport`] in `Json`; the single-line
/// `format_one_line` rendering in `Human`. `action` is the verb stem
/// (`"Ingested"`, `"OCR-ingested"`, ...) and `label` is the noun the
/// operator can recognise (typically a file basename).
pub fn emit_job_summary(report: &JobOutcomeReport, action: &str, label: &str) {
    if ctx().is_quiet() {
        return;
    }
    if ctx().is_json() {
        match serde_json::to_string_pretty(report) {
            Ok(text) => println!("{text}"),
            Err(_) => println!("{{}}"),
        }
        return;
    }
    println!("{}", report.format_one_line(action, label));
}

/// Pretty-print a JSON value on stdout.
pub fn print_value(value: &Value) {
    if ctx().is_quiet() {
        return;
    }
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("{value}"),
    }
}

/// Drive the two-step pinned destructive RPC protocol used by
/// `corpus.rebuild`, `vectors.reembed`, `remove`, and their paper
/// peers. Sends the dry-run leg with `selectors`, prints the
/// structured plan, then — unless the user passed `--dry-run` or
/// declined a confirmation prompt — sends the execute leg with the
/// returned `plan_id` and prints the outcome.
///
/// `selectors` is the JSON object that names what the dry-run should
/// plan for (e.g. `{ "book": 7, "stale_only": true }`). The helper
/// merges in `dry_run = true` for the first call and
/// `{ yes = true, plan_id = … }` for the second.
///
/// When `user_yes` is `false` the helper prompts via
/// [`bookrack_cli::render::confirm::confirm_destructive`] in `Soft`
/// mode; an empty / declined answer aborts before the execute leg
/// runs.
pub async fn run_pinned_destructive(
    client: std::sync::Arc<ControlClient>,
    method: &str,
    mut selectors: Value,
    user_dry_run: bool,
    user_yes: bool,
    confirm_prompt: &str,
) -> Result<()> {
    use bookrack_cli::render::confirm::{ConfirmMode, confirm_destructive};

    selectors["dry_run"] = Value::Bool(true);
    let plan = call_with_progress_value(client.clone(), method, selectors).await?;
    print_value(&plan);

    if user_dry_run {
        return Ok(());
    }

    let plan_id = plan
        .get("plan_id")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| {
            eyre::eyre!("{method}: daemon dry-run response did not include a plan_id")
        })?;

    let confirmed = confirm_destructive(confirm_prompt, ConfirmMode::Soft, user_yes)
        .context("read destructive-action confirmation")?;
    if !confirmed {
        println!("aborted; no changes written");
        return Ok(());
    }

    let execute_params = serde_json::json!({
        "yes": true,
        "plan_id": plan_id,
    });
    let outcome = call_with_progress_value(client, method, execute_params).await?;
    print_value(&outcome);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    fn tick_without_finished() -> Event {
        Event {
            channel: "queue.tick".to_string(),
            value: json!({ "pending": 1, "running": 0 }),
            lag: false,
        }
    }

    /// Loose default for in-test waits: long enough that the event
    /// loop can't race the timer in CI, short enough that the stall
    /// test still finishes promptly.
    const TEST_STALL_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn await_jobs_returns_immediately_when_empty() {
        let (_tx, rx) = broadcast::channel::<Event>(4);
        let report = await_jobs_from_rx(rx, Vec::new(), Instant::now(), TEST_STALL_TIMEOUT)
            .await
            .unwrap();
        assert!(report.jobs.is_empty());
        assert_eq!(report.totals.done, 0);
    }

    #[tokio::test]
    async fn await_jobs_collects_all_three_terminal_states() {
        let (tx, rx) = broadcast::channel::<Event>(16);
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let handle = tokio::spawn(async move {
            await_jobs_from_rx(rx, ids, Instant::now(), TEST_STALL_TIMEOUT).await
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
        let ids = vec!["target".to_string()];
        let handle = tokio::spawn(async move {
            await_jobs_from_rx(rx, ids, Instant::now(), TEST_STALL_TIMEOUT).await
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
        let ids = vec!["a".to_string()];
        let started = Instant::now();
        let err = await_jobs_from_rx(rx, ids, started, Duration::from_millis(50))
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
    }

    #[tokio::test]
    async fn await_jobs_errors_when_stream_closes_with_pending() {
        let (tx, rx) = broadcast::channel::<Event>(4);
        let ids = vec!["a".to_string()];
        let handle = tokio::spawn(async move {
            await_jobs_from_rx(rx, ids, Instant::now(), TEST_STALL_TIMEOUT).await
        });
        drop(tx);
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("1 job"));
    }

    #[test]
    fn extract_finished_filters_by_pending_set() {
        let mut pending = HashSet::new();
        pending.insert("a".to_string());
        let ev = tick("a", "done", 0, 0).value;
        assert!(extract_finished(&ev, &pending).is_some());
        let other = tick("b", "done", 0, 0).value;
        assert!(extract_finished(&other, &pending).is_none());
    }
}
