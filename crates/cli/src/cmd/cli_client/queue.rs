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
//!
//! `queue list` renders a table in human mode (id8, kind, state,
//! queued (relative), priority, path basename) and the full daemon
//! payload under `--json`. `--long` prints the full UUIDv7 in the id
//! column when the operator needs a copy-paste-able id. The pause /
//! resume / clear / cancel actions remain thin pass-throughs over
//! the matching RPC.

use std::path::PathBuf;

use bookrack_cli::render::ctx;
use bookrack_cli::render::human::{basename_or_dash, short_id};
use bookrack_cli::render::table::RowTable;
use bookrack_cli::render::time::relative_from_iso;
use bookrack_cli_grammar::QueueAction;
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;

pub async fn run(action: QueueAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        QueueAction::List { long } => {
            let response = helpers::dispatch(&client, "queue.list", Value::Null).await?;
            if ctx().is_json() {
                helpers::print_value(&response);
                return Ok(());
            }
            if ctx().is_quiet() {
                return Ok(());
            }
            render_queue_list(&response, long);
            Ok(())
        }
        QueueAction::Pause => helpers::call_and_print(&client, "queue.pause", Value::Null).await,
        QueueAction::Resume => helpers::call_and_print(&client, "queue.resume", Value::Null).await,
        QueueAction::Clear => helpers::call_and_print(&client, "queue.clear", Value::Null).await,
        QueueAction::Cancel { job_id } => {
            helpers::call_and_print(&client, "ingest.cancel", json!({ "job_id": job_id })).await
        }
    }
}

fn render_queue_list(response: &Value, long: bool) {
    let paused = response
        .get("paused")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let jobs = response.get("jobs").and_then(Value::as_array);
    match jobs {
        Some(jobs) if !jobs.is_empty() => {
            let mut table = RowTable::new(["id", "kind", "state", "queued", "priority", "path"]);
            for job in jobs {
                let id = job.get("id").and_then(Value::as_str).unwrap_or("");
                let id_cell = if long {
                    id.to_string()
                } else {
                    short_id(id).to_string()
                };
                let kind = job.get("kind").and_then(Value::as_str).unwrap_or("-");
                let state = job.get("state").and_then(Value::as_str).unwrap_or("-");
                let queued = job
                    .get("queued_at")
                    .and_then(Value::as_str)
                    .map(relative_from_iso)
                    .unwrap_or_else(|| "-".to_string());
                let priority = job.get("priority").and_then(Value::as_str).unwrap_or("-");
                let path = job
                    .get("path")
                    .and_then(Value::as_str)
                    .map(|p| basename_or_dash(Some(p)).to_string())
                    .unwrap_or_else(|| "-".to_string());
                table.push_row([
                    id_cell,
                    kind.to_string(),
                    state.to_string(),
                    queued,
                    priority.to_string(),
                    path,
                ]);
            }
            println!("{}", table.render());
        }
        _ => {
            println!("queue is empty");
        }
    }
    if paused {
        println!("queue is paused");
    }
}
