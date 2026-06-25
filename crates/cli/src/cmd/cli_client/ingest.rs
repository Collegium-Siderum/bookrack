//! `bookrack ingest <path>` — submit a single file to the daemon's
//! ingest queue and stream worker progress until the job lands.

use std::path::PathBuf;

use bookrack_cli_grammar::IngestArgs;
use eyre::Result;
use serde_json::json;

use super::helpers;

pub async fn run(args: IngestArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "paths": [args.path],
        "force": args.force,
        "recursive": args.recursive,
        "hold_for_metadata": args.hold_for_metadata,
    });
    helpers::call_with_progress(client, "ingest.submit", params).await
}
