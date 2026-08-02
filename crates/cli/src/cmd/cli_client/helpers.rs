//! Shared connection, rendering, and progress plumbing for the
//! one-shot CLI clients in this module tree.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bookrack_cli::daemon_call::{DEFAULT_AWAIT_STALL_TIMEOUT, DEFAULT_CALL_TIMEOUT};
use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::confirm::{ConfirmMode, Confirmation};
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

/// Render the per-batch summary and translate a non-success outcome
/// into a typed [`BookrackCliError::IngestPartialFailure`] so the
/// binary exits with code `5` instead of `0`. Centralised here so
/// every `await_jobs` caller shares the same surface; per-job
/// detail in the rendered summary is the operator-facing diagnostic.
pub fn finalize_job_batch(report: &JobOutcomeReport, action: &str, label: &str) -> Result<()> {
    emit_job_summary(report, action, label);
    if report.all_succeeded() {
        Ok(())
    } else {
        Err(BookrackCliError::IngestPartialFailure {
            failed: report.totals.failed,
            cancelled: report.totals.cancelled,
            total: report.jobs.len() as u32,
        }
        .into())
    }
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
/// mode; a declined answer aborts before the execute leg runs. A stdin
/// that carries no answer at all is a user error rather than a
/// decline, so a caller whose stdin is closed cannot read exit 0 out
/// of a run that changed nothing.
pub async fn run_pinned_destructive(
    client: std::sync::Arc<ControlClient>,
    method: &str,
    selectors: Value,
    user_dry_run: bool,
    user_yes: bool,
    confirm_prompt: &str,
) -> Result<()> {
    use bookrack_cli::render::confirm::{ConfirmMode, confirm_destructive};

    run_pinned_destructive_with(
        client,
        method,
        selectors,
        user_dry_run,
        confirm_prompt,
        |text| confirm_destructive(text, ConfirmMode::Soft, user_yes),
    )
    .await
}

/// [`run_pinned_destructive`] with the confirmation step supplied by
/// the caller, so the two-leg protocol can be driven without a
/// terminal. `confirm` receives the prompt text and answers for the
/// operator; anything but [`Confirmation::Agreed`] must leave the
/// execute leg unsent.
async fn run_pinned_destructive_with<F>(
    client: std::sync::Arc<ControlClient>,
    method: &str,
    mut selectors: Value,
    user_dry_run: bool,
    confirm_prompt: &str,
    confirm: F,
) -> Result<()>
where
    F: FnOnce(&str) -> std::io::Result<Confirmation>,
{
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

    let confirmed = confirm(confirm_prompt)
        .context("read destructive-action confirmation")?
        .agreed_or_refuse(method, "re-run with --yes to confirm without a prompt")?;
    if !confirmed {
        eprintln!("aborted; no changes written");
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

/// Outcome of the pre-prompt decision made by
/// [`destructive_confirmation_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestructiveConfirmation {
    /// The caller has already vouched for the operation
    /// (`--yes`) or it is on a daemon-exempt path (e.g. `resume`);
    /// no prompt is shown.
    Skip,
    /// The operator must confirm interactively before the RPC fires.
    Prompt,
}

/// Pure decision for whether a destructive command needs an
/// interactive confirmation prompt. Factored out so the matrix of
/// `(user_yes, confirmation_exempt)` cases is unit-testable without
/// touching stdin.
pub fn destructive_confirmation_decision(
    user_yes: bool,
    confirmation_exempt: bool,
) -> DestructiveConfirmation {
    if user_yes || confirmation_exempt {
        DestructiveConfirmation::Skip
    } else {
        DestructiveConfirmation::Prompt
    }
}

/// Bundle of prompt-shaped strings for [`run_destructive`]. Groups
/// the three knobs that describe "how does the operator see this
/// confirmation" so the function signature stays readable.
pub struct DestructivePrompt<'a> {
    /// Confirmation strength — `Soft` (`yes`/`y`) or
    /// `Hard { token: "RESET" }`.
    pub mode: ConfirmMode<'a>,
    /// Multi-line text written to stderr before reading the
    /// confirmation. The final line should be the actual prompt
    /// (e.g. `Type 'yes' to continue:`).
    pub text: &'a str,
    /// Remedy offered when stdin carried no answer. Should explain
    /// what the command does and how to opt in without a prompt.
    pub unanswered_hint: &'a str,
}

/// One-shot destructive RPC wrapper for methods that do not need a
/// pinned plan id (`vectors.reset`, `vectors.drop`, and their paper
/// peers). Sister of [`run_pinned_destructive`].
///
/// `params` MUST be a JSON object; the helper merges `yes: true` into
/// it before dispatching, so call sites should construct it as
/// `json!({})` or `json!({ "resume": resume })` rather than
/// `Value::Null`. The assertion at the top of the function catches the
/// misuse loudly instead of silently turning a malformed payload into
/// `{ "yes": true }`.
///
/// Confirmation behaviour:
///
/// * When `user_yes` (i.e. `--yes`) or `confirmation_exempt` (e.g.
///   `--resume` on `reset`) is set, the prompt is skipped and the RPC
///   fires immediately with `yes: true`.
/// * Otherwise the helper writes `prompt.text` to stderr through
///   [`bookrack_cli::render::confirm::confirm_destructive`] in
///   `prompt.mode`; a rejected answer prints `aborted; no changes
///   written` and returns `Ok(())` without firing the RPC, while a
///   stdin that carries no answer at all is a user error carrying
///   `prompt.unanswered_hint`.
///
/// A caller without a terminal is asked like any other. Bounding the
/// read so an idle pipe cannot park the command is
/// `render::confirm`'s job, not this one.
pub async fn run_destructive(
    client: Arc<ControlClient>,
    method: &str,
    params: Value,
    user_yes: bool,
    confirmation_exempt: bool,
    prompt: DestructivePrompt<'_>,
) -> Result<()> {
    use bookrack_cli::render::confirm::confirm_destructive;

    run_destructive_with(
        client,
        method,
        params,
        user_yes,
        confirmation_exempt,
        prompt,
        |text, mode| confirm_destructive(text, mode, false),
    )
    .await
}

/// [`run_destructive`] with the confirmation step supplied by the
/// caller, so each outcome — and the RPC it does or does not emit —
/// is observable without a terminal.
async fn run_destructive_with<F>(
    client: Arc<ControlClient>,
    method: &str,
    mut params: Value,
    user_yes: bool,
    confirmation_exempt: bool,
    prompt: DestructivePrompt<'_>,
    confirm: F,
) -> Result<()>
where
    F: FnOnce(&str, ConfirmMode<'_>) -> std::io::Result<Confirmation>,
{
    assert!(
        params.is_object(),
        "run_destructive: params must be a JSON object; got {params:?}"
    );

    match destructive_confirmation_decision(user_yes, confirmation_exempt) {
        DestructiveConfirmation::Skip => {}
        DestructiveConfirmation::Prompt => {
            let confirmed = confirm(prompt.text, prompt.mode)
                .with_context(|| format!("read {method} confirmation"))?
                .agreed_or_refuse(method, prompt.unanswered_hint)?;
            if !confirmed {
                eprintln!("aborted; no changes written");
                return Ok(());
            }
        }
    }

    params
        .as_object_mut()
        .expect("params verified as an object above")
        .insert("yes".to_string(), Value::Bool(true));

    call_with_progress(client, method, params).await
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

    #[test]
    fn extract_finished_recognises_skipped_duplicate_state() {
        // Sanity-guard the wire-token contract between the daemon and
        // the CLI wait path: a job whose ingest short-circuited on the
        // noop-if-up-to-date path must be extracted as a terminal
        // outcome, otherwise `bookrack ingest --wait` would silently
        // spin until the stall timeout when every job was a dedup skip.
        let mut pending = HashSet::new();
        pending.insert("a".to_string());
        let ev = tick("a", "skipped_duplicate", 0, 0).value;
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
        let ev = tick("a", "needs_ocr", 0, 0).value;
        let record = extract_finished(&ev, &pending).expect("needs_ocr terminal recognised");
        assert!(matches!(record.state, JobOutcomeState::NeedsOcr));
    }

    #[test]
    fn destructive_decision_matrix() {
        use DestructiveConfirmation::{Prompt, Skip};
        assert_eq!(destructive_confirmation_decision(false, false), Prompt);
        assert_eq!(destructive_confirmation_decision(true, false), Skip);
        assert_eq!(destructive_confirmation_decision(false, true), Skip);
        assert_eq!(destructive_confirmation_decision(true, true), Skip);
    }

    fn rec(id: &str, state: JobOutcomeState) -> JobOutcomeRecord {
        JobOutcomeRecord {
            job_id: id.into(),
            kind: "book".into(),
            state,
            error: None,
            finished_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn finalize_job_batch_returns_ok_when_every_job_succeeded() {
        let report = JobOutcomeReport::new(
            vec![
                rec("a", JobOutcomeState::Done),
                rec("b", JobOutcomeState::Done),
            ],
            Duration::from_millis(0),
        );
        finalize_job_batch(&report, "Ingested", "label").expect("all-done batch returns Ok");
    }

    #[test]
    fn finalize_job_batch_surfaces_typed_error_for_failed_jobs() {
        let report = JobOutcomeReport::new(
            vec![
                rec("a", JobOutcomeState::Done),
                rec("b", JobOutcomeState::Failed),
            ],
            Duration::from_millis(0),
        );
        let err = finalize_job_batch(&report, "Ingested", "label").unwrap_err();
        let cli = err
            .downcast_ref::<BookrackCliError>()
            .expect("typed CLI error must surface directly");
        match cli {
            BookrackCliError::IngestPartialFailure {
                failed,
                cancelled,
                total,
            } => {
                assert_eq!(*failed, 1);
                assert_eq!(*cancelled, 0);
                assert_eq!(*total, 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        assert_eq!(cli.exit_code(), 5);
    }

    #[test]
    fn finalize_job_batch_surfaces_typed_error_for_cancelled_jobs() {
        let report = JobOutcomeReport::new(
            vec![rec("a", JobOutcomeState::Cancelled)],
            Duration::from_millis(0),
        );
        let err = finalize_job_batch(&report, "Ingested", "label").unwrap_err();
        let cli = err.downcast_ref::<BookrackCliError>().unwrap();
        assert!(matches!(
            cli,
            BookrackCliError::IngestPartialFailure {
                failed: 0,
                cancelled: 1,
                total: 1,
            }
        ));
    }

    #[test]
    fn finalize_job_batch_error_survives_context_wrappers_for_classify_eyre() {
        use bookrack_cli::error::classify_eyre;

        let report = JobOutcomeReport::new(
            vec![rec("a", JobOutcomeState::Failed)],
            Duration::from_millis(0),
        );
        let err = finalize_job_batch(&report, "Ingested", "label")
            .unwrap_err()
            .wrap_err("await ingest jobs")
            .wrap_err("bookrack ingest");
        let cause = classify_eyre(&err).expect("typed CLI error must be reachable");
        assert!(matches!(
            cause.as_cli(),
            BookrackCliError::IngestPartialFailure { .. }
        ));
        assert_eq!(cause.as_cli().exit_code(), 5);
    }
}

/// Wire-level tests for the destructive helpers: what the confirmation
/// gate decides has to show up as requests that do or do not reach the
/// daemon, so the assertions run against a stub control socket and read
/// back everything that arrived on it.
///
/// Unix-only because the stub speaks the Unix-domain half of the
/// transport; the platform-independent decision logic is covered by
/// [`tests::destructive_decision_matrix`] above.
#[cfg(all(test, unix))]
mod destructive_wire_tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use bookrack_cli::render::confirm::{ConfirmMode, Confirmation};
    use bookrack_control_client::{ControlClient, ControlSocket};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;

    use super::{DestructivePrompt, run_destructive_with, run_pinned_destructive_with};

    /// A control socket that answers every request with one canned
    /// result and records the `(method, params)` pairs it received, in
    /// arrival order.
    struct StubDaemon {
        socket: PathBuf,
        seen: Arc<Mutex<Vec<(String, Value)>>>,
        _dir: tempfile::TempDir,
    }

    impl StubDaemon {
        /// Bind a fresh socket and start answering. Every request is
        /// recorded *before* its reply is written, so a completed
        /// round-trip proves every earlier request on the connection
        /// is already recorded.
        fn start(result: Value) -> Self {
            let dir = tempfile::tempdir().expect("stub socket dir");
            let socket = dir.path().join("control.sock");
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind stub socket");
            let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
            let seen_for_task = Arc::clone(&seen);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let seen = Arc::clone(&seen_for_task);
                    let result = result.clone();
                    tokio::spawn(async move {
                        let (read, mut write) = tokio::io::split(stream);
                        let mut lines = BufReader::new(read).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            let id = request["id"].as_u64().unwrap_or_default();
                            let method = request["method"].as_str().unwrap_or_default().to_string();
                            seen.lock().await.push((method, request["params"].clone()));
                            let mut frame = serde_json::to_vec(&json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": result,
                            }))
                            .expect("encode stub reply");
                            frame.push(b'\n');
                            if write.write_all(&frame).await.is_err() {
                                break;
                            }
                            let _ = write.flush().await;
                        }
                    });
                }
            });
            Self {
                socket,
                seen,
                _dir: dir,
            }
        }

        async fn client(&self) -> Arc<ControlClient> {
            let socket = ControlSocket::from_path(&self.socket);
            Arc::new(
                bookrack_control_client::connect(&socket)
                    .await
                    .expect("connect to the stub control socket"),
            )
        }

        /// Round-trip a sentinel request so the connection is drained,
        /// then return every request the stub saw, minus the sentinel
        /// and the event subscription that `call_with_progress` opens.
        /// Absence in the returned list therefore means "never sent",
        /// not "not yet arrived".
        async fn drain(&self, client: &ControlClient) -> Vec<(String, Value)> {
            client
                .call_raw("stub.sentinel", json!({}))
                .await
                .expect("stub answers the sentinel");
            self.seen
                .lock()
                .await
                .iter()
                .filter(|(method, _)| method != "stub.sentinel" && method != "events.subscribe")
                .cloned()
                .collect()
        }
    }

    fn prompt() -> DestructivePrompt<'static> {
        DestructivePrompt {
            mode: ConfirmMode::Hard { token: "RESET" },
            text: "Type RESET to continue:",
            unanswered_hint: "vectors reset drops the existing vectors; pass --yes to confirm",
        }
    }

    #[tokio::test]
    async fn run_destructive_declined_sends_nothing() {
        let stub = StubDaemon::start(json!({"ok": true}));
        let client = stub.client().await;
        run_destructive_with(
            Arc::clone(&client),
            "vectors.reset",
            json!({}),
            false,
            false,
            prompt(),
            |_, _| Ok(Confirmation::Declined),
        )
        .await
        .expect("a declined confirmation is not an error");
        assert!(
            stub.drain(&client).await.is_empty(),
            "a declined confirmation must not put the destructive RPC on the wire"
        );
    }

    #[tokio::test]
    async fn run_destructive_confirmed_dispatches_with_yes_merged() {
        let stub = StubDaemon::start(json!({"ok": true}));
        let client = stub.client().await;
        run_destructive_with(
            Arc::clone(&client),
            "vectors.reset",
            json!({"resume": false}),
            false,
            false,
            prompt(),
            |_, _| Ok(Confirmation::Agreed),
        )
        .await
        .expect("a confirmed reset dispatches");
        let calls = stub.drain(&client).await;
        assert_eq!(
            calls,
            vec![(
                "vectors.reset".to_string(),
                json!({"resume": false, "yes": true})
            )],
            "the confirmed call is dispatched exactly once, with yes merged in"
        );
    }

    /// A caller without a terminal is asked, not refused. The window
    /// that keeps an idle pipe from parking the command lives in
    /// `render::confirm`; the helper's job is only to put the question
    /// and act on the answer, so an answer that arrives over a pipe
    /// dispatches exactly as one typed at a terminal would.
    #[tokio::test]
    async fn run_destructive_prompts_a_non_tty_rather_than_refusing_it() {
        let stub = StubDaemon::start(json!({"ok": true}));
        let client = stub.client().await;
        let asked = std::cell::Cell::new(false);
        run_destructive_with(
            Arc::clone(&client),
            "vectors.reset",
            json!({}),
            false,
            false,
            prompt(),
            |_, _| {
                asked.set(true);
                Ok(Confirmation::Agreed)
            },
        )
        .await
        .expect("an answered prompt dispatches");
        assert!(asked.get(), "the operator must be asked, not refused");
        assert_eq!(
            stub.drain(&client).await,
            vec![("vectors.reset".to_string(), json!({"yes": true}))],
            "an answer that agrees dispatches regardless of where it came from"
        );
    }

    /// When the answer cannot be obtained at all, the caller's hint is
    /// what reaches the operator and nothing reaches the daemon.
    #[tokio::test]
    async fn run_destructive_unanswerable_sends_nothing_and_is_a_user_error() {
        use bookrack_cli::render::confirm::NoAnswer;

        let stub = StubDaemon::start(json!({"ok": true}));
        let client = stub.client().await;
        let err = run_destructive_with(
            Arc::clone(&client),
            "vectors.reset",
            json!({}),
            false,
            false,
            prompt(),
            |_, _| Ok(Confirmation::Unanswerable(NoAnswer::EndOfStream)),
        )
        .await
        .expect_err("a confirmation nobody answered is a user error");
        let cli = bookrack_cli::error::classify_eyre(&err).expect("the typed error must survive");
        assert_eq!(cli.as_cli().exit_code(), 2);
        assert!(
            err.to_string().contains("pass --yes to confirm"),
            "the refusal must carry the caller's directed hint: {err:#}"
        );
        assert!(
            stub.drain(&client).await.is_empty(),
            "an unanswered call must not reach the daemon"
        );
    }

    #[tokio::test]
    async fn run_destructive_skips_the_prompt_when_the_caller_consented() {
        for (user_yes, confirmation_exempt) in [(true, false), (false, true)] {
            let stub = StubDaemon::start(json!({"ok": true}));
            let client = stub.client().await;
            run_destructive_with(
                Arc::clone(&client),
                "vectors.reset",
                json!({}),
                user_yes,
                confirmation_exempt,
                prompt(),
                |_, _| panic!("consent already given; the prompt must not run"),
            )
            .await
            .expect("consent dispatches without a prompt");
            assert_eq!(
                stub.drain(&client).await,
                vec![("vectors.reset".to_string(), json!({"yes": true}))],
            );
        }
    }

    #[tokio::test]
    async fn pinned_declined_sends_the_dry_run_leg_only() {
        let stub = StubDaemon::start(json!({"plan_id": "plan-7", "books": 3}));
        let client = stub.client().await;
        run_pinned_destructive_with(
            Arc::clone(&client),
            "corpus.rebuild",
            json!({"stale_only": true}),
            false,
            "Type 'yes' to continue:",
            |_| Ok(Confirmation::Declined),
        )
        .await
        .expect("a declined confirmation is not an error");
        assert_eq!(
            stub.drain(&client).await,
            vec![(
                "corpus.rebuild".to_string(),
                json!({"stale_only": true, "dry_run": true})
            )],
            "declining must leave the execute leg unsent"
        );
    }

    /// A stdin that carries no answer leaves the execute leg unsent —
    /// same as a decline — but the run is a user error, not a clean
    /// abort. The confirmation sits behind the dry-run RPC, so this is
    /// the only place the mapping can be asserted through the real
    /// two-leg entry point.
    #[tokio::test]
    async fn pinned_unanswerable_sends_the_dry_run_leg_only_and_is_a_user_error() {
        use bookrack_cli::render::confirm::NoAnswer;

        let stub = StubDaemon::start(json!({"plan_id": "plan-7", "books": 3}));
        let client = stub.client().await;
        let err = run_pinned_destructive_with(
            Arc::clone(&client),
            "corpus.rebuild",
            json!({"stale_only": true}),
            false,
            "Type 'yes' to continue:",
            |_| Ok(Confirmation::Unanswerable(NoAnswer::EndOfStream)),
        )
        .await
        .expect_err("an unanswerable confirmation is a user error");
        let cli = bookrack_cli::error::classify_eyre(&err)
            .expect("the typed CLI error must survive the context wrappers");
        assert!(
            matches!(
                cli.as_cli(),
                bookrack_cli::error::BookrackCliError::ConfirmationUnanswerable { .. }
            ),
            "unexpected variant: {:?}",
            cli.as_cli()
        );
        assert_eq!(cli.as_cli().exit_code(), 2);
        assert_eq!(
            stub.drain(&client).await,
            vec![(
                "corpus.rebuild".to_string(),
                json!({"stale_only": true, "dry_run": true})
            )],
            "an unanswered confirmation must leave the execute leg unsent"
        );
    }

    #[tokio::test]
    async fn pinned_confirmed_executes_against_the_plan_id_the_dry_run_returned() {
        let stub = StubDaemon::start(json!({"plan_id": "plan-7", "books": 3}));
        let client = stub.client().await;
        run_pinned_destructive_with(
            Arc::clone(&client),
            "corpus.rebuild",
            json!({"stale_only": true}),
            false,
            "Type 'yes' to continue:",
            |_| Ok(Confirmation::Agreed),
        )
        .await
        .expect("a confirmed rebuild executes");
        assert_eq!(
            stub.drain(&client).await,
            vec![
                (
                    "corpus.rebuild".to_string(),
                    json!({"stale_only": true, "dry_run": true})
                ),
                (
                    "corpus.rebuild".to_string(),
                    json!({"yes": true, "plan_id": "plan-7"})
                ),
            ],
            "the execute leg carries the plan id the daemon pinned, not the selectors"
        );
    }

    #[tokio::test]
    async fn pinned_dry_run_stops_before_the_confirmation() {
        let stub = StubDaemon::start(json!({"plan_id": "plan-7"}));
        let client = stub.client().await;
        run_pinned_destructive_with(
            Arc::clone(&client),
            "corpus.rebuild",
            json!({"book": 4}),
            true,
            "Type 'yes' to continue:",
            |_| panic!("--dry-run must return before any confirmation"),
        )
        .await
        .expect("a dry run reports the plan and stops");
        assert_eq!(
            stub.drain(&client).await,
            vec![(
                "corpus.rebuild".to_string(),
                json!({"book": 4, "dry_run": true})
            )],
        );
    }

    /// A daemon that answers the dry run without a `plan_id` is a
    /// protocol violation: the helper must fail loudly rather than
    /// prompt for an execute leg it cannot pin.
    #[tokio::test]
    async fn pinned_rejects_a_dry_run_response_without_a_plan_id() {
        let stub = StubDaemon::start(json!({"books": 3}));
        let client = stub.client().await;
        let err = run_pinned_destructive_with(
            Arc::clone(&client),
            "corpus.rebuild",
            json!({}),
            false,
            "Type 'yes' to continue:",
            |_| panic!("an unpinnable plan must not reach the confirmation"),
        )
        .await
        .expect_err("a plan-less dry-run response is an error");
        assert!(
            err.to_string().contains("did not include a plan_id"),
            "unexpected message: {err:#}"
        );
    }
}
