//! `bookrack stamps reconcile` — control-plane wrapper.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_cli_grammar::StampsAction;
use serde_json::Value;

use super::helpers;

pub async fn run(action: StampsAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let StampsAction::Reconcile = action;
    helpers::call_and_print(&client, "stamps.reconcile", Value::Null).await
}
