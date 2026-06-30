// SPDX-License-Identifier: Apache-2.0

//! `bookrack intake` — one-shot control-plane client for derived-source
//! intake commands. Currently only `intake ocr`, which enqueues an OCR
//! markdown product against its scan PDF anchor onto the persistent
//! ingest queue. The daemon runs the actual pipeline; this client
//! follows worker progress until the job leaves the queue.

use std::path::PathBuf;

use bookrack_cli::render::human::basename_or_dash;
use bookrack_cli_grammar::IntakeAction;
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;

pub async fn run(
    action: IntakeAction,
    runtime_dir: Option<PathBuf>,
    audit_profile: Option<String>,
) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        IntakeAction::Ocr {
            ocr_md,
            from_pdf,
            expected_pages,
            allow_partial,
            force,
            hold_for_metadata,
            priority,
            no_wait,
        } => {
            // Subscribe before issuing the RPC so a fast worker's
            // `queue.tick` cannot fire between submit-ack and the
            // wait loop's first `recv`.
            let rx = client
                .subscribe()
                .await
                .map_err(eyre::Report::from)
                .map_err(|e| e.wrap_err("subscribe to control-plane events"))?;

            let label = basename_or_dash(ocr_md.to_str()).to_string();
            let mut params = json!({
                "ocr_md": ocr_md,
                "from_pdf": from_pdf,
                "allow_partial": allow_partial,
                "force": force,
                "hold_for_metadata": hold_for_metadata,
            });
            if let Some(pages) = expected_pages {
                params["expected_pages"] = Value::from(pages);
            }
            if let Some(name) = audit_profile {
                params["audit_profile"] = Value::String(name);
            }
            if let Some(level) = priority {
                params["priority"] = Value::String(level);
            }

            let response = helpers::dispatch(&client, "intake.ocr", params).await?;
            let job_ids = helpers::extract_job_ids(&response);

            if no_wait || job_ids.is_empty() {
                helpers::print_value(&response);
                return Ok(());
            }

            let report = helpers::await_jobs(rx, &job_ids).await?;
            helpers::emit_job_summary(&report, "OCR-ingested", &label);
            Ok(())
        }
    }
}
