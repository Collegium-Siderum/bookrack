//! `bookrack remove` — control-plane wrapper.
//!
//! Drives the daemon's two-step pinned destructive RPC: send
//! `remove` with `dry_run = true`, print the plan, prompt
//! (unless `--yes`), then send the execute leg under the returned
//! `plan_id` so the daemon acts on exactly the intake the operator
//! confirmed.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::RemoveArgs;
use serde_json::json;

use super::helpers;

pub async fn run(args: RemoveArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let selectors = json!({
        "intake_id": args.intake_id,
        "sha": args.sha,
    });
    helpers::run_pinned_destructive(
        client,
        "remove",
        selectors,
        args.dry_run,
        args.yes,
        "About to delete this book from every store. This is\n\
         irreversible (vector tombstones are not recoverable).\n\
         Audit rows are preserved. Type 'yes' to continue: ",
    )
    .await
}
