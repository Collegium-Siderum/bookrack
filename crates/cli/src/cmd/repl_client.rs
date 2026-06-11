//! `bookrack repl` — the standalone control-socket client.
//!
//! Connects to a running daemon over the control socket, hosts a
//! reedline editor in the client process, and dispatches every command
//! as a JSON-RPC call. The client does not hold the session lock, does
//! not open the data root, and does not run the runtime stack: every
//! side effect that ships out happens on the daemon side.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use bookrack_control_client::{ControlClient, ControlError, Event};
use bookrack_repl_grammar::{
    CorpusAction, IntakeAction, QueueAction, ReplCli, ReplCommand, StampsAction,
    WriteMetadataAction, WriteVectorsAction,
};
use clap::Parser;
use reedline::{
    FileBackedHistory, History, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, Reedline, Signal,
};
use serde_json::{Value, json};
use tokio::runtime::Handle;

const HISTORY_FILE: &str = ".bookrack-history";
const HISTORY_CAPACITY: usize = 1000;

/// Exit code the binary returns when the daemon can't be reached.
const EXIT_NOT_RUNNING: i32 = 2;
/// Exit code the binary returns when a batch line fails.
const EXIT_BATCH_FAILURE: i32 = 1;

/// Entry point invoked from `main::run`. Resolves the daemon address,
/// opens the control socket, subscribes to the event stream, and then
/// switches into interactive or batch mode depending on whether stdin
/// is a tty.
pub async fn run(runtime_dir: Option<PathBuf>) -> Result<()> {
    let socket = match bookrack_control_client::discover(runtime_dir.as_deref()) {
        Ok(socket) => socket,
        Err(ControlError::NotRunning) => not_running_exit(),
        Err(err) => {
            eprintln!("bookrack: resolve daemon address: {err}");
            std::process::exit(EXIT_NOT_RUNNING);
        }
    };
    let client = match bookrack_control_client::connect(&socket).await {
        Ok(client) => Arc::new(client),
        Err(ControlError::NotRunning) => not_running_exit(),
        Err(err) => {
            eprintln!("bookrack: connect to {}: {err}", socket.path().display());
            std::process::exit(EXIT_NOT_RUNNING);
        }
    };
    let status = Arc::new(RwLock::new(StatusSnapshot::default()));
    bootstrap_status(&client, &status).await;
    let events = client
        .subscribe()
        .await
        .context("subscribe to control-plane events")?;
    tokio::spawn(event_loop(events, Arc::clone(&status)));

    if std::io::stdin().is_terminal() {
        run_interactive(Arc::clone(&client), Arc::clone(&status), runtime_dir).await
    } else {
        run_batch(Arc::clone(&client)).await
    }
}

fn not_running_exit() -> ! {
    eprintln!("bookrack daemon not running; start it with: bookrack run");
    std::process::exit(EXIT_NOT_RUNNING);
}

/// Latest known state from the event stream. Read by the prompt
/// renderer on every redraw; written by the background event loop.
#[derive(Debug, Default, Clone)]
struct StatusSnapshot {
    library: Option<String>,
    queue_pending: u32,
    state: PromptState,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum PromptState {
    #[default]
    Idle,
    Writing,
    Degraded,
    Disconnected,
}

impl PromptState {
    fn indicator(self) -> &'static str {
        match self {
            PromptState::Idle => "",
            PromptState::Writing => "*",
            PromptState::Degraded => "!",
            PromptState::Disconnected => "?",
        }
    }
}

async fn bootstrap_status(client: &ControlClient, status: &Arc<RwLock<StatusSnapshot>>) {
    let channels = json!({
        "channels": ["daemon.state", "queue.tick", "library.changed"],
    });
    let snapshot = match client.call_raw("events.snapshot", channels).await {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!(error = %err, "bootstrap snapshot failed");
            return;
        }
    };
    if let Ok(mut guard) = status.write() {
        if let Some(state_value) = snapshot.get("daemon.state") {
            apply_daemon_state(&mut guard, state_value);
        }
        if let Some(tick_value) = snapshot.get("queue.tick") {
            apply_queue_tick(&mut guard, tick_value);
        }
        if let Some(lib_value) = snapshot.get("library.changed") {
            apply_library(&mut guard, lib_value);
        }
    }
}

async fn event_loop(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    status: Arc<RwLock<StatusSnapshot>>,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                if event.lag {
                    if let Ok(mut guard) = status.write() {
                        guard.state = PromptState::Disconnected;
                    }
                    continue;
                }
                if let Ok(mut guard) = status.write() {
                    match event.channel.as_str() {
                        "daemon.state" => apply_daemon_state(&mut guard, &event.value),
                        "queue.tick" => apply_queue_tick(&mut guard, &event.value),
                        "library.changed" => apply_library(&mut guard, &event.value),
                        _ => {}
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                if let Ok(mut guard) = status.write() {
                    guard.state = PromptState::Disconnected;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                if let Ok(mut guard) = status.write() {
                    guard.state = PromptState::Disconnected;
                }
                break;
            }
        }
    }
}

fn apply_daemon_state(snapshot: &mut StatusSnapshot, value: &Value) {
    let raw = value.as_str().unwrap_or("");
    snapshot.state = match raw {
        "writing" => PromptState::Writing,
        "degraded" => PromptState::Degraded,
        "stopping" | "idle" => PromptState::Idle,
        _ => snapshot.state,
    };
}

fn apply_queue_tick(snapshot: &mut StatusSnapshot, value: &Value) {
    if let Some(pending) = value.get("pending").and_then(Value::as_u64) {
        snapshot.queue_pending = pending as u32;
    }
}

fn apply_library(snapshot: &mut StatusSnapshot, value: &Value) {
    if let Some(name) = value.get("library").and_then(Value::as_str) {
        snapshot.library = Some(name.to_string());
    }
}

async fn run_interactive(
    client: Arc<ControlClient>,
    status: Arc<RwLock<StatusSnapshot>>,
    runtime_dir_override: Option<PathBuf>,
) -> Result<()> {
    let history_dir = bookrack_session::resolve_runtime_dir(runtime_dir_override.as_deref())
        .context("resolve runtime dir for history file")?;
    let handle = Handle::current();
    let editor_task = tokio::task::spawn_blocking(move || -> Result<()> {
        let history_path = history_dir.join(HISTORY_FILE);
        let history: Box<dyn History> = match FileBackedHistory::with_file(
            HISTORY_CAPACITY,
            history_path.clone(),
        ) {
            Ok(h) => Box::new(h),
            Err(err) => {
                eprintln!(
                    "bookrack: history file {} unavailable ({err}); session running without history",
                    history_path.display()
                );
                Box::<FileBackedHistory>::default()
            }
        };
        let mut editor = Reedline::create().with_history(history);
        let prompt = BookrackPrompt {
            status: Arc::clone(&status),
        };
        loop {
            match editor.read_line(&prompt) {
                Ok(Signal::Success(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if matches!(trimmed, "exit" | "quit") {
                        break;
                    }
                    let _ = handle.block_on(dispatch_line(&client, trimmed));
                }
                Ok(Signal::CtrlD) => {
                    println!();
                    break;
                }
                Ok(Signal::CtrlC) => {
                    println!("^C  (type `exit` or Ctrl-D to leave)");
                    continue;
                }
                Ok(_) => continue,
                Err(err) => {
                    eprintln!("bookrack: REPL read_line error: {err}");
                    break;
                }
            }
        }
        Ok(())
    });
    editor_task.await.context("repl editor task panicked")??;
    Ok(())
}

async fn run_batch(client: Arc<ControlClient>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if matches!(trimmed, "exit" | "quit") {
            break;
        }
        if !dispatch_line(&client, trimmed).await {
            std::process::exit(EXIT_BATCH_FAILURE);
        }
    }
    Ok(())
}

/// Evaluate one tokenised input line. Returns `true` when the line
/// either succeeded or was a benign no-op (`help`, malformed shell
/// quoting); returns `false` when an RPC call failed so batch mode can
/// surface a non-zero exit.
async fn dispatch_line(client: &ControlClient, line: &str) -> bool {
    let tokens = match shlex::split(line) {
        Some(tokens) if !tokens.is_empty() => tokens,
        Some(_) => return true,
        None => {
            eprintln!("bookrack: cannot parse input (unclosed quote?)");
            return true;
        }
    };
    match tokens[0].as_str() {
        "help" => {
            print_repl_help();
            return true;
        }
        "status" => return call_and_print(client, "status", Value::Null).await,
        "libs" => return call_and_print(client, "library.list", Value::Null).await,
        "queue" => return handle_queue(client, &tokens).await,
        "logs" => {
            print_phase_unavailable("logs.tail");
            return true;
        }
        "use" => {
            print_phase_unavailable("library.set_default");
            return true;
        }
        _ => {}
    }
    match ReplCli::try_parse_from(&tokens) {
        Ok(repl_cli) => dispatch_repl_command(client, repl_cli.command).await,
        Err(err) => {
            let _ = err.print();
            true
        }
    }
}

async fn handle_queue(client: &ControlClient, tokens: &[String]) -> bool {
    match tokens.get(1).map(String::as_str) {
        Some("list" | "ls") | None => match client.call_raw("queue.list", Value::Null).await {
            Ok(value) => {
                render_queue_list(&value);
                true
            }
            Err(err) => {
                eprintln!("queue.list: {err}");
                false
            }
        },
        Some("cancel") => match tokens.get(2) {
            Some(prefix) => {
                let params = json!({"job_id": prefix});
                call_and_print(client, "ingest.cancel", params).await
            }
            None => {
                eprintln!("usage: queue cancel <job-id-prefix>");
                true
            }
        },
        Some("add") => {
            let mut paths: Vec<PathBuf> = Vec::new();
            let mut library: Option<String> = None;
            let mut priority: Option<String> = None;
            let mut force = false;
            let mut i = 2;
            while i < tokens.len() {
                match tokens[i].as_str() {
                    "--library" => match tokens.get(i + 1) {
                        Some(v) => {
                            library = Some(v.clone());
                            i += 2;
                        }
                        None => {
                            eprintln!("queue add: --library requires a value");
                            return true;
                        }
                    },
                    "--priority" => match tokens.get(i + 1) {
                        Some(v) => {
                            priority = Some(v.clone());
                            i += 2;
                        }
                        None => {
                            eprintln!("queue add: --priority requires a value");
                            return true;
                        }
                    },
                    "--force" => {
                        force = true;
                        i += 1;
                    }
                    other if other.starts_with("--") => {
                        eprintln!("queue add: unknown flag {other}");
                        return true;
                    }
                    other => {
                        paths.push(PathBuf::from(other));
                        i += 1;
                    }
                }
            }
            if paths.is_empty() {
                eprintln!("queue add: missing path");
                return true;
            }
            let mut params = json!({"paths": paths, "force": force});
            if let Some(lib) = library {
                params["library"] = Value::String(lib);
            }
            if let Some(pri) = priority {
                params["priority"] = Value::String(pri);
            }
            call_and_print(client, "ingest.submit", params).await
        }
        Some("clear") => call_and_print(client, "queue.clear", Value::Null).await,
        Some("pause") => call_and_print(client, "queue.pause", Value::Null).await,
        Some("resume") => call_and_print(client, "queue.resume", Value::Null).await,
        Some(other) => {
            eprintln!("queue: unknown subcommand {other:?}");
            true
        }
    }
}

async fn dispatch_repl_command(client: &ControlClient, command: ReplCommand) -> bool {
    match command {
        ReplCommand::Ingest(args) => {
            if args.recursive {
                eprintln!(
                    "bookrack: --recursive is not yet wired over the control plane; submit individual files or use `queue add`",
                );
                return true;
            }
            if args.hold_for_metadata {
                eprintln!(
                    "bookrack: --hold-for-metadata is not yet wired over the control plane; the daemon proceeds without holding",
                );
            }
            let params = json!({"paths": [args.path], "force": args.force});
            call_and_print(client, "ingest.submit", params).await
        }
        ReplCommand::Intake {
            action: IntakeAction::Ocr { .. },
        } => {
            eprintln!(
                "intake ocr: not yet available over the control plane; run via `bookrack intake ocr ...` after Phase 4",
            );
            true
        }
        ReplCommand::Metadata { action } => dispatch_metadata(client, action).await,
        ReplCommand::Vectors { action } => dispatch_vectors(client, action).await,
        ReplCommand::Corpus {
            action:
                CorpusAction::Rebuild {
                    include_vectors,
                    book,
                    stale_only,
                    dry_run,
                    yes,
                },
        } => {
            let params = json!({
                "include_vectors": include_vectors,
                "book": book,
                "stale_only": stale_only,
                "dry_run": dry_run,
                "yes": yes,
            });
            call_and_print(client, "corpus.rebuild", params).await
        }
        ReplCommand::Stamps {
            action: StampsAction::Reconcile,
        } => call_and_print(client, "stamps.reconcile", Value::Null).await,
        ReplCommand::Remove(args) => {
            let params = json!({
                "intake_id": args.intake_id,
                "sha": args.sha,
                "dry_run": args.dry_run,
                "yes": args.yes,
            });
            call_and_print(client, "remove", params).await
        }
        ReplCommand::Dryrun(args) => {
            let params = json!({
                "path": args.path,
                "out": args.out,
                "stdout": args.stdout,
                "no_chunk": args.no_chunk,
            });
            call_and_print(client, "dryrun", params).await
        }
        ReplCommand::Queue { action } => match action {
            QueueAction::Pause => call_and_print(client, "queue.pause", Value::Null).await,
            QueueAction::Resume => call_and_print(client, "queue.resume", Value::Null).await,
            QueueAction::Clear => call_and_print(client, "queue.clear", Value::Null).await,
        },
    }
}

async fn dispatch_metadata(client: &ControlClient, action: WriteMetadataAction) -> bool {
    match action {
        WriteMetadataAction::Set {
            book,
            field,
            value,
            reason,
            confirmed,
        } => {
            call_and_print(
                client,
                "metadata.set",
                json!({
                    "book": book,
                    "field": field,
                    "value": value,
                    "reason": reason,
                    "confirmed": confirmed,
                }),
            )
            .await
        }
        WriteMetadataAction::Clear {
            book,
            field,
            reason,
        } => {
            call_and_print(
                client,
                "metadata.clear",
                json!({"book": book, "field": field, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Void {
            book,
            field,
            reason,
        } => {
            call_and_print(
                client,
                "metadata.void",
                json!({"book": book, "field": field, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Reaudit { book } => {
            call_and_print(client, "metadata.reaudit", json!({"book": book})).await
        }
        WriteMetadataAction::ContributorAdd {
            book,
            role,
            name,
            nationality,
            reason,
        } => {
            call_and_print(
                client,
                "metadata.contributor_add",
                json!({
                    "book": book,
                    "role": role,
                    "name": name,
                    "nationality": nationality,
                    "reason": reason,
                }),
            )
            .await
        }
        WriteMetadataAction::ContributorRemove {
            book,
            contributor_id,
            reason,
        } => {
            call_and_print(
                client,
                "metadata.contributor_remove",
                json!({"book": book, "contributor_id": contributor_id, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Ack { book, reason } => {
            call_and_print(
                client,
                "metadata.ack",
                json!({"book": book, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Approve { book, reason } => {
            call_and_print(
                client,
                "metadata.approve",
                json!({"book": book, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Reject { book, reason } => {
            call_and_print(
                client,
                "metadata.reject",
                json!({"book": book, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Advance { book: _ } => {
            eprintln!(
                "metadata advance: not yet available over the control plane; run via `bookrack metadata advance ...` after Phase 4",
            );
            true
        }
    }
}

async fn dispatch_vectors(client: &ControlClient, action: WriteVectorsAction) -> bool {
    match action {
        WriteVectorsAction::Rebuild {
            kind,
            num_partitions,
            num_sub_vectors,
            num_bits,
            nprobes,
            refine_factor,
        } => {
            let params = json!({
                "kind": kind,
                "num_partitions": num_partitions,
                "num_sub_vectors": num_sub_vectors,
                "num_bits": num_bits,
                "nprobes": nprobes,
                "refine_factor": refine_factor,
            });
            call_and_print(client, "vectors.rebuild", params).await
        }
        WriteVectorsAction::Drop => call_and_print(client, "vectors.drop", Value::Null).await,
        WriteVectorsAction::Reembed {
            book,
            stale_only,
            dry_run,
            yes,
        } => {
            let params = json!({
                "book": book,
                "stale_only": stale_only,
                "dry_run": dry_run,
                "yes": yes,
            });
            call_and_print(client, "vectors.reembed", params).await
        }
        WriteVectorsAction::Reset { yes, resume } => {
            match crate::util::confirm_vectors_reset(yes, resume) {
                Ok(true) => {}
                Ok(false) => {
                    println!("aborted; no changes written");
                    return true;
                }
                Err(err) => {
                    eprintln!("bookrack: {err:#}");
                    return false;
                }
            }
            let params = json!({"yes": true, "resume": resume});
            call_and_print(client, "vectors.reset", params).await
        }
    }
}

async fn call_and_print(client: &ControlClient, method: &str, params: Value) -> bool {
    match client.call_raw(method, params).await {
        Ok(value) => {
            print_value(&value);
            true
        }
        Err(err) => {
            eprintln!("{method}: {err}");
            false
        }
    }
}

fn print_value(value: &Value) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("{value}"),
    }
}

fn render_queue_list(value: &Value) {
    if let Some(paused) = value.get("paused").and_then(Value::as_bool)
        && paused
    {
        println!("queue: PAUSED");
    }
    let jobs = match value.get("jobs").and_then(Value::as_array) {
        Some(jobs) => jobs,
        None => {
            print_value(value);
            return;
        }
    };
    if jobs.is_empty() {
        println!("(queue is empty)");
        return;
    }
    println!(
        "{:<10}  {:<9}  {:<12}  {:<40}  QUEUED",
        "ID", "STATE", "LIBRARY", "FILE",
    );
    for job in jobs {
        let id = job
            .get("id")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(8).collect::<String>())
            .unwrap_or_default();
        let state = job
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let library = job
            .get("library")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let file = job
            .get("path")
            .and_then(Value::as_str)
            .and_then(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_default();
        let queued = job
            .get("queued_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        println!(
            "{:<10}  {:<9}  {:<12}  {:<40}  {queued}",
            id, state, library, file,
        );
    }
}

fn print_phase_unavailable(method: &str) {
    eprintln!("bookrack: {method} is not yet wired through the control plane in this phase",);
}

fn print_repl_help() {
    println!("Built-in commands:");
    println!("  exit, quit       Leave the REPL");
    println!("  help             Show this help");
    println!("  status           Show the daemon's lifecycle state and queue counts");
    println!("  libs             List the libraries known to the daemon");
    println!("  queue add <path> [--library X] [--priority {{low|normal|high}}] [--force]");
    println!("  queue list       Show the daemon's persisted queue");
    println!("  queue cancel <id-prefix>");
    println!();
    println!("Write commands (`ingest`, `metadata`, `vectors`, `corpus`, `stamps`,");
    println!("`remove`, `dryrun`) are parsed against the same grammar as the embedded");
    println!("REPL; each variant maps to the matching control-plane RPC.");
    println!();
}

/// Reedline [`Prompt`] backed by the shared status snapshot. The
/// renderer reads on each redraw, so a queue-tick event surfaces in
/// the next prompt line without explicit repaint plumbing.
struct BookrackPrompt {
    status: Arc<RwLock<StatusSnapshot>>,
}

impl Prompt for BookrackPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let snapshot = match self.status.read() {
            Ok(guard) => guard.clone(),
            Err(_) => StatusSnapshot::default(),
        };
        let library = snapshot.library.as_deref().unwrap_or("?");
        let indicator = snapshot.state.indicator();
        Cow::Owned(format!(
            "{indicator}bookrack:{library}/queue:{}",
            snapshot.queue_pending,
        ))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(":: ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({prefix}reverse-i-search: '{}') ",
            history_search.term
        ))
    }
}
