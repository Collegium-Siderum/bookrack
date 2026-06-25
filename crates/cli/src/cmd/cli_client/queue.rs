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
//! Reads emit the daemon's JSON payload verbatim; writes print the
//! `{ ok: true }` response from the handler.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_cli_grammar::QueueAction;
use serde_json::{Value, json};

use super::helpers;

pub async fn run(action: QueueAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    match action {
        QueueAction::List => helpers::call_and_print(&client, "queue.list", Value::Null).await,
        QueueAction::Pause => helpers::call_and_print(&client, "queue.pause", Value::Null).await,
        QueueAction::Resume => helpers::call_and_print(&client, "queue.resume", Value::Null).await,
        QueueAction::Clear => helpers::call_and_print(&client, "queue.clear", Value::Null).await,
        QueueAction::Cancel { job_id } => {
            helpers::call_and_print(&client, "ingest.cancel", json!({ "job_id": job_id })).await
        }
    }
}
