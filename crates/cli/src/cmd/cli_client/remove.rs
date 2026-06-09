//! `bookrack remove` — control-plane wrapper.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::RemoveArgs;
use serde_json::json;

use super::helpers;

pub async fn run(args: RemoveArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "intake_id": args.intake_id,
        "sha": args.sha,
        "dry_run": args.dry_run,
        "yes": args.yes,
    });
    helpers::call_and_print(&client, "remove", params).await
}
