//! Shared connection, rendering, and progress plumbing for the
//! one-shot CLI clients in this module tree.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bookrack_cli::daemon_call::DEFAULT_CALL_TIMEOUT;
use bookrack_cli::error::BookrackCliError;
use bookrack_cli::library_param;
use bookrack_cli::path_sugar::{self, Routing};
use bookrack_cli::render::confirm::{ConfirmMode, Confirmation};
use bookrack_cli::render::job_report::JobOutcomeReport;
use bookrack_cli::render::{OutputMode, ctx};
use bookrack_control_client::{ControlClient, ControlError};
use eyre::{Context, Result};
use serde_json::Value;
use tokio::sync::broadcast;

use super::progress_view::ProgressView;

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
    let client =
        match bookrack_control_client::connect_with_default_timeout(&socket, DEFAULT_CALL_TIMEOUT)
            .await
        {
            Ok(client) => Arc::new(client),
            Err(ControlError::NotRunning) => return Err(BookrackCliError::DaemonNotRunning.into()),
            Err(source) => return Err(BookrackCliError::DaemonUnreachable { source }.into()),
        };
    settle_library_selection(&client).await?;
    Ok(client)
}

/// How long to wait for a daemon to answer a best-effort question
/// nobody asked for. Matches the health-probe window `bookrack status`
/// uses, so "is a daemon there" is decided on the same budget wherever
/// it is asked.
// setting: internal -- the wait on a question the operator did not ask;
// exceeding it drops one annotation column, so there is nothing to tune
const SERVED_PROBE_WINDOW: Duration = Duration::from_secs(2);

/// Names of the libraries a running daemon serves, or `None` when no
/// daemon answered.
///
/// Deliberately below both gates the ordinary call path goes through:
///
/// * **no selection settling** — the answer is the whole served set, on
///   which the operator's selection has no bearing. Going through
///   [`connect`] would send a path-shaped selection to the daemon for
///   translation, so a command that only wanted to annotate a listing
///   could fail on a selection it never used;
/// * **no selection injection** — `library.list` takes none, and this
///   call is the command's own rather than the operator's.
///
/// Every failure collapses to `None`: no daemon, an unreachable socket,
/// a daemon that does not answer inside [`SERVED_PROBE_WINDOW`], or a
/// reply in an unexpected shape. A caller therefore cannot tell an
/// unserved library from an unanswered question, and must render
/// neither as the other.
pub async fn served_library_names(runtime_dir: Option<&Path>) -> Option<Vec<String>> {
    let socket = bookrack_control_client::discover(runtime_dir).ok()?;
    let names = tokio::time::timeout(SERVED_PROBE_WINDOW, async move {
        let client =
            bookrack_control_client::connect_with_default_timeout(&socket, SERVED_PROBE_WINDOW)
                .await
                .ok()?;
        let value = client.call_raw("library.list", Value::Null).await.ok()?;
        Some(
            value
                .as_array()?
                .iter()
                .filter_map(|row| row["name"].as_str().map(String::from))
                .collect::<Vec<String>>(),
        )
    })
    .await
    .ok()??;
    Some(names)
}

/// Decide, once a connection exists, which library name this
/// invocation puts on its calls.
///
/// A name needs no deciding. A path does: the registry translates it
/// into the name of the library that claims that root, and a root no
/// entry claims has no name to send — which is ordinary, and means the
/// daemon serving it was started on that same path. Whether *this*
/// daemon is that one is the only thing left to ask, and only the
/// daemon can answer it, so the question is asked here rather than at
/// startup: a command that never connects never needed it, and one
/// that finds no daemon has a better thing to report.
///
/// The probe calls `status` directly rather than through [`dispatch`]:
/// the selection it exists to settle is not settled yet, and `status`
/// is one of the methods that refuses a selection.
pub(super) async fn settle_library_selection(client: &ControlClient) -> Result<()> {
    let Some(selection) = library_param::pending() else {
        return Ok(());
    };
    let name = match path_sugar::routing_for(selection)? {
        Routing::Unselected => None,
        Routing::Named(name) => Some(name),
        Routing::UnclaimedRoot(asked) => {
            let status = client
                .call_raw("status", Value::Null)
                .await
                .context("status rpc")?;
            let served = status.get("data_dir").and_then(Value::as_str);
            match served {
                Some(served) if same_root(Path::new(served), &asked) => None,
                Some(served) => {
                    return Err(BookrackCliError::RootNotRoutable {
                        problem: path_sugar::serves_another_root(&asked, Path::new(served)),
                    }
                    .into());
                }
                // A daemon that does not say which root it serves
                // cannot be shown to be serving another one. Refusing
                // on that would turn a missing field into an accusation.
                None => None,
            }
        }
    };
    library_param::init(name);
    Ok(())
}

/// Whether two paths name the same root, comparing canonicalized forms
/// and falling back to a raw comparison when canonicalization fails —
/// the same rule the config crate matches registry entries by.
fn same_root(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The daemon's `status` payload.
///
/// Calls `status` directly rather than through [`dispatch`]: `status`
/// reports on the process itself and is one of the methods that refuses
/// a library selection. Its callers read process-level fields only —
/// the queue verbs act on the process, and `index-profile apply` names
/// its own target on every action it sends afterwards — so a selection
/// has nowhere to go here.
pub(super) async fn daemon_status(client: &ControlClient) -> Result<Value> {
    client
        .call_raw("status", Value::Null)
        .await
        .context("status rpc")
}

/// Whether a `status` payload reports a queue worker. A payload without
/// the field reads as a worker present.
pub(super) fn reports_queue_worker(status: &Value) -> bool {
    status
        .get("queue_worker_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Whether the daemon runs a queue worker, read off [`daemon_status`].
pub(super) async fn queue_worker_enabled(client: &ControlClient) -> Result<bool> {
    Ok(reports_queue_worker(&daemon_status(client).await?))
}

/// Send one JSON-RPC request and return the `result` payload.
///
/// Pure RPC: no event subscription, no rendering, no printing. The
/// building block underneath every other call helper in this module
/// and the unit of work subcommands compose with the wait in
/// `job_wait` when they want to wait for queue completion.
pub async fn dispatch(client: &ControlClient, method: &str, params: Value) -> Result<Value> {
    let params = library_param::apply(method, params)?;
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
    // The second chokepoint: this one calls `call_raw` itself rather
    // than going through `dispatch`, and every long-running write
    // arrives here. Injecting in one place only would leave exactly
    // the calls that act on a library without a library.
    let params = library_param::apply(method, params)?;
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
    let mut view = ProgressView::for_call(method);
    let value = loop {
        tokio::select! {
            biased;
            res = &mut call_future => break res?,
            ev = events.recv() => match ev {
                Ok(event) => {
                    view.apply(&event);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    break (&mut call_future).await?;
                }
            },
        }
    };
    Ok(value)
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
/// into a typed [`BookrackCliError::JobBatchPartialFailure`] so the
/// binary exits with code `5` instead of `0`. Centralised here so
/// every waiting command shares the same surface; per-job
/// detail in the rendered summary is the operator-facing diagnostic.
/// `scope` names the waiting command in the error.
pub fn finalize_job_batch(
    report: &JobOutcomeReport,
    scope: &'static str,
    action: &str,
    label: &str,
) -> Result<()> {
    emit_job_summary(report, action, label);
    if report.all_succeeded() {
        Ok(())
    } else {
        Err(BookrackCliError::JobBatchPartialFailure {
            scope,
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
    println!("{}", pretty(value));
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Print the outcome of one write in the mode the operator asked for.
/// See [`outcome_line`] for what each mode prints.
pub fn emit_outcome(response: &Value, sentence: Option<String>) {
    if let Some(text) = outcome_line(ctx().output(), response, sentence) {
        println!("{text}");
    }
}

/// Output-mode gate shared by every write that renders its reply as a
/// sentence: `Quiet` prints nothing and lets the exit code answer,
/// `Json` prints the server's response payload so a script parses one
/// object, `Human` prints `sentence`.
///
/// A caller builds `sentence` only when the reply carries every field
/// the sentence states, and passes `None` otherwise; `Human` then
/// prints the payload as `Json` does, so a reply from a daemon that
/// does not report a field is shown as it is rather than worded with
/// a value nobody sent.
pub fn outcome_line(
    mode: OutputMode,
    response: &Value,
    sentence: Option<String>,
) -> Option<String> {
    match (mode, sentence) {
        (OutputMode::Quiet, _) => None,
        (OutputMode::Human, Some(sentence)) => Some(sentence),
        (OutputMode::Json | OutputMode::Human, _) => Some(pretty(response)),
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

/// What the operator sees around one [`run_destructive`] call: the
/// confirmation it asks for, and how the reply is worded once the call
/// succeeds. Grouped so the function signature stays readable.
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
    /// The human-mode sentence for the reply, or `None` when the reply
    /// lacks a field the sentence states; see [`emit_outcome`].
    pub outcome: fn(&Value) -> Option<String>,
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
///
/// The reply is printed through [`emit_outcome`], worded by
/// `prompt.outcome`.
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

    let reply = call_with_progress_value(client, method, params).await?;
    emit_outcome(&reply, (prompt.outcome)(&reply));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_cli::render::job_report::{JobOutcomeRecord, JobOutcomeState};
    use bookrack_runtime::control::methods::REGISTRY;
    use serde_json::json;

    /// Every method name this module tree sends must be one the
    /// daemon answers.
    ///
    /// The two tables have different granularity — `rpc call` forwards
    /// whatever the operator types, and a few call sites build their
    /// method from a match — so the relation asserted is containment,
    /// not equality. Reading the call sites out of the source is what
    /// makes it a check on the surface rather than on a list somebody
    /// remembered to update.
    #[test]
    fn every_method_name_the_client_sends_is_one_the_daemon_answers() {
        let sent = method_literals_in_call_position();
        assert!(
            sent.len() >= 40,
            "found only {} call sites; the scan stopped seeing them",
            sent.len()
        );
        for (file, method) in &sent {
            assert!(
                REGISTRY.iter().any(|sig| sig.name == method),
                "{file} sends `{method}`, which the daemon's method table does not carry"
            );
        }
    }

    /// The selection gate lives on the path to `call_raw`, so a call
    /// site that reaches for `call_raw` on its own is a call that can
    /// skip it.
    ///
    /// Two files legitimately do: `doctor` sits between two
    /// no-daemon fallbacks and `quit` swallows the shutdown race.
    /// Both pass the gate anyway, and this asserts that they do —
    /// an exemption from the call shape is not an exemption from the
    /// rules.
    #[test]
    fn call_raw_stays_behind_the_selection_gate() {
        let mut offenders: Vec<String> = Vec::new();
        for (name, source) in client_sources() {
            if !source.contains("call_raw(") || name == "helpers.rs" {
                continue;
            }
            if !matches!(name.as_str(), "doctor.rs" | "quit.rs") {
                offenders.push(format!("{name} calls call_raw outside helpers"));
                continue;
            }
            assert!(
                source.contains("library_param::apply("),
                "{name} calls call_raw without passing the selection gate"
            );
        }
        assert!(offenders.is_empty(), "{}", offenders.join("; "));
    }

    /// `(file, method)` for every string literal sitting in the method
    /// argument of a control-plane call. A site whose method comes
    /// from a variable contributes nothing — the literal behind it, if
    /// any, is out of reach from here.
    fn method_literals_in_call_position() -> Vec<(String, String)> {
        const CALLS: [&str; 5] = [
            "dispatch(",
            "call_and_print(",
            "call_with_progress(",
            "call_with_progress_value(",
            "call_raw(",
        ];
        let mut found = Vec::new();
        for (name, source) in client_sources() {
            for call in CALLS {
                let mut from = 0;
                while let Some(at) = source[from..].find(call) {
                    let start = from + at + call.len();
                    from = start;
                    let window = &source[start..source.len().min(start + 200)];
                    let Some(open) = window.find('"') else {
                        continue;
                    };
                    let Some(len) = window[open + 1..].find('"') else {
                        continue;
                    };
                    let literal = &window[open + 1..open + 1 + len];
                    if literal.contains('.')
                        && literal
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_')
                    {
                        found.push((name.clone(), literal.to_string()));
                    }
                }
            }
        }
        found
    }

    /// The source of every client module, read from the crate the test
    /// is compiled in rather than from the current directory.
    fn client_sources() -> Vec<(String, String)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/cli_client");
        let mut sources = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read the client module directory") {
            let path = entry.expect("read a client module").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("client module file name")
                .to_string();
            let source = std::fs::read_to_string(&path).expect("read a client module");
            // Production call sites only: a test in one of these files
            // may name a method no daemon answers precisely because it
            // is testing what happens then.
            let source = match source.find("\n#[cfg(test)]\nmod tests") {
                Some(at) => source[..at].to_string(),
                None => source,
            };
            sources.push((name, source));
        }
        assert!(!sources.is_empty(), "no client modules found at {dir:?}");
        sources
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
        finalize_job_batch(&report, "ingest", "Ingested", "label")
            .expect("all-done batch returns Ok");
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
        let err = finalize_job_batch(&report, "ingest", "Ingested", "label").unwrap_err();
        let cli = err
            .downcast_ref::<BookrackCliError>()
            .expect("typed CLI error must surface directly");
        match cli {
            BookrackCliError::JobBatchPartialFailure {
                scope: "ingest",
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
        let err = finalize_job_batch(&report, "ingest", "Ingested", "label").unwrap_err();
        let cli = err.downcast_ref::<BookrackCliError>().unwrap();
        assert!(matches!(
            cli,
            BookrackCliError::JobBatchPartialFailure {
                scope: "ingest",
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
        let err = finalize_job_batch(&report, "ingest", "Ingested", "label")
            .unwrap_err()
            .wrap_err("await ingest jobs")
            .wrap_err("bookrack ingest");
        let cause = classify_eyre(&err).expect("typed CLI error must be reachable");
        assert!(matches!(
            cause.as_cli(),
            BookrackCliError::JobBatchPartialFailure { .. }
        ));
        assert_eq!(cause.as_cli().exit_code(), 5);
    }
    const SENTENCE: &str = "Cleared override on title for paper 7.";

    fn write_response() -> Value {
        json!({ "removed": true, "field": "title", "intake_id": 7 })
    }

    #[test]
    fn a_write_outcome_prints_nothing_in_quiet_mode() {
        assert_eq!(
            outcome_line(
                OutputMode::Quiet,
                &write_response(),
                Some(SENTENCE.to_string())
            ),
            None
        );
        assert_eq!(
            outcome_line(OutputMode::Quiet, &write_response(), None),
            None
        );
    }

    #[test]
    fn a_write_outcome_prints_the_server_payload_in_json_mode() {
        for sentence in [Some(SENTENCE.to_string()), None] {
            let line = outcome_line(OutputMode::Json, &write_response(), sentence)
                .expect("json mode prints the payload");
            assert!(
                !line.contains(SENTENCE),
                "the human sentence reached a --json stream:\n{line}"
            );
            let parsed: Value =
                serde_json::from_str(&line).expect("json mode prints one JSON object");
            assert_eq!(parsed, write_response());
        }
    }

    #[test]
    fn a_write_outcome_prints_the_sentence_in_human_mode() {
        assert_eq!(
            outcome_line(
                OutputMode::Human,
                &write_response(),
                Some(SENTENCE.to_string())
            ),
            Some(SENTENCE.to_string())
        );
    }

    /// A reply too thin to word is shown as it came, not as a sentence
    /// with a value filled in.
    #[test]
    fn a_write_outcome_without_a_sentence_prints_the_payload_in_human_mode() {
        let line = outcome_line(OutputMode::Human, &write_response(), None)
            .expect("human mode prints something");
        let parsed: Value = serde_json::from_str(&line).expect("the payload, as JSON");
        assert_eq!(parsed, write_response());
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
            outcome: |_| None,
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
