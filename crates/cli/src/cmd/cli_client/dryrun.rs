//! `bookrack dryrun` — control-plane wrapper.

use std::path::PathBuf;

use bookrack_cli_grammar::DryrunArgs;
use eyre::{Context, Result};
use serde_json::json;

use super::helpers;

pub async fn run(args: DryrunArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "path": args.path,
        "out": args.out,
        "no_chunk": args.no_chunk,
    });
    let value = helpers::call_with_progress_value(client, "dryrun", params).await?;
    let outcome: bookrack_runtime::cmd::dryrun::DryrunRunOutcome = serde_json::from_value(value)
        .context("dryrun response did not match the expected shape")?;
    bookrack_runtime::cmd::dryrun::render_outcome(&outcome, args.stdout)
}
