//! `bookrack ingest <path>` — submit a file (or a recursive walk's
//! worth of files) to the daemon's ingest queue, then stay attached
//! until every enqueued job lands in a terminal state.

use std::path::PathBuf;

use bookrack_cli::render::human::basename_or_dash;
use bookrack_cli_grammar::IngestArgs;
use eyre::Result;
use serde_json::json;

use super::helpers;

pub async fn run(args: IngestArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;

    // Subscribe before issuing the RPC so `queue.tick` events fired
    // by the worker between submit-ack and the wait loop's first
    // `recv` cannot slip past us.
    let rx = client
        .subscribe()
        .await
        .map_err(eyre::Report::from)
        .map_err(|e| e.wrap_err("subscribe to control-plane events"))?;

    let params = json!({
        "paths": [args.path],
        "force": args.force,
        "recursive": args.recursive,
        "hold_for_metadata": args.hold_for_metadata,
    });
    let response = helpers::dispatch(&client, "ingest.submit", params).await?;
    let job_ids = helpers::extract_job_ids(&response);

    if args.no_wait || job_ids.is_empty() {
        helpers::print_value(&response);
        return Ok(());
    }

    let report = helpers::await_jobs(rx, &job_ids).await?;
    let label = basename_or_dash(args.path.to_str());
    helpers::emit_job_summary(&report, "Ingested", label);
    Ok(())
}
