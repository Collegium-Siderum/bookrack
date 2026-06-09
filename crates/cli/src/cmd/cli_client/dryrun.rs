//! `bookrack dryrun` — control-plane wrapper.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::DryrunArgs;
use serde_json::json;

use super::helpers;

pub async fn run(args: DryrunArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "path": args.path,
        "out": args.out,
        "stdout": args.stdout,
        "no_chunk": args.no_chunk,
    });
    helpers::call_with_progress(client, "dryrun", params).await
}
