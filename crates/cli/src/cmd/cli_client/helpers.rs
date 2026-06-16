//! Shared connection, rendering, and progress plumbing for the
//! one-shot CLI clients in this module tree.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bookrack_control_client::{ControlClient, ControlError, Event};
use serde_json::Value;
use tokio::sync::broadcast;

/// Exit code the binary returns when the daemon is unreachable, or
/// when a clap parse fails. Matches `bookrack repl`.
pub const EXIT_NOT_RUNNING: i32 = 2;

/// Discover the daemon and open a control-plane connection. On
/// `ControlError::NotRunning` the process exits with code 2 after
/// printing a stable stderr message — matches the contract in
/// `bookrack repl`.
pub async fn connect_or_exit(runtime_dir: Option<&Path>) -> Arc<ControlClient> {
    let socket = match bookrack_control_client::discover(runtime_dir) {
        Ok(socket) => socket,
        Err(ControlError::NotRunning) => not_running_exit(),
        Err(err) => {
            eprintln!("bookrack: resolve daemon address: {err}");
            std::process::exit(EXIT_NOT_RUNNING);
        }
    };
    match bookrack_control_client::connect(&socket).await {
        Ok(client) => Arc::new(client),
        Err(ControlError::NotRunning) => not_running_exit(),
        Err(err) => {
            eprintln!("bookrack: connect to {}: {err}", socket.path().display());
            std::process::exit(EXIT_NOT_RUNNING);
        }
    }
}

fn not_running_exit() -> ! {
    eprintln!("bookrack daemon not running; start it with: bookrack run");
    std::process::exit(EXIT_NOT_RUNNING);
}

/// Call the named RPC, await the response, and pretty-print the
/// `result` on stdout.
pub async fn call_and_print(client: &ControlClient, method: &str, params: Value) -> Result<()> {
    let value = client
        .call_raw(method, params)
        .await
        .with_context(|| format!("{method} rpc"))?;
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
            .map_err(anyhow::Error::from)
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
                    // Broadcast closed; finish the call to surface
                    // any remaining error.
                    break (&mut call_future).await?;
                }
            },
        }
    };
    finish_progress_line();
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
            .map_err(anyhow::Error::from)
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

/// Render one broadcast [`Event`] to stderr. Worker progress lines
/// rewrite the same row with `\r`; the other channels are dropped to
/// keep the one-shot output legible.
pub fn render_event(event: &Event) {
    if event.lag {
        eprintln!("\nbookrack: event stream lagged; progress may be incomplete");
        return;
    }
    if event.channel != "worker.progress" {
        return;
    }
    let value = &event.value;
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

/// Emit a trailing newline after the progress row so the final
/// stdout payload starts on a fresh line.
pub fn finish_progress_line() {
    eprintln!();
}

/// Pretty-print a JSON value on stdout.
pub fn print_value(value: &Value) {
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
/// [`crate::util::confirm`]; an empty / declined answer aborts
/// before the execute leg runs.
pub async fn run_pinned_destructive(
    client: std::sync::Arc<ControlClient>,
    method: &str,
    mut selectors: Value,
    user_dry_run: bool,
    user_yes: bool,
    confirm_prompt: &str,
) -> Result<()> {
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
            anyhow::anyhow!("{method}: daemon dry-run response did not include a plan_id")
        })?;

    if !user_yes && !crate::util::confirm(confirm_prompt)? {
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
