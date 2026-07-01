// SPDX-License-Identifier: Apache-2.0

//! `bookrack intake` — one-shot control-plane client for derived-source
//! intake commands: `intake ocr`, which enqueues an OCR markdown product
//! against its scan PDF anchor onto the persistent ingest queue, and
//! `intake list-ocr-pending`, the read-side worklist of scan sources
//! still awaiting OCR. The daemon runs the actual pipeline; this client
//! follows worker progress until the job leaves the queue.

use std::path::PathBuf;

use bookrack_cli::render::human::{basename_or_dash, truncate_to};
use bookrack_cli::render::table::RowTable;
use bookrack_cli_grammar::IntakeAction;
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;

pub async fn run(
    action: IntakeAction,
    runtime_dir: Option<PathBuf>,
    audit_profile: Option<String>,
    json: bool,
) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        IntakeAction::ListOcrPending { limit, offset } => {
            let mut params = json!({});
            if let Some(n) = limit {
                params["limit"] = Value::from(n);
            }
            if let Some(n) = offset {
                params["offset"] = Value::from(n);
            }
            let response = helpers::dispatch(&client, "library.list_ocr_pending", params).await?;
            if json {
                helpers::print_value(&response);
            } else {
                render_ocr_pending(&response);
            }
            Ok(())
        }
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
            helpers::finalize_job_batch(&report, "OCR-ingested", &label)
        }
    }
}

/// Render an `OcrPendingResult` as a human table: one row per scan
/// source awaiting OCR, followed by a count line. The `source_path` is
/// shown by its basename; the full path is in the `--json` manifest.
fn render_ocr_pending(response: &Value) {
    let items = response.get("items").and_then(Value::as_array);
    match items {
        Some(items) if !items.is_empty() => {
            let mut table = RowTable::new(["intake", "pages", "source", "reason"]);
            for item in items {
                let intake_id = item
                    .get("intake_id")
                    .and_then(Value::as_i64)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let pages = item
                    .get("pages")
                    .and_then(Value::as_i64)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let source = item
                    .get("source_path")
                    .and_then(Value::as_str)
                    .map(|p| basename_or_dash(Some(p)).to_string())
                    .unwrap_or_else(|| "-".to_string());
                let reason = item
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|r| truncate_to(r, 60))
                    .unwrap_or_else(|| "-".to_string());
                table.push_row([intake_id, pages, source, reason]);
            }
            println!("{}", table.render());
        }
        _ => {
            println!("no sources awaiting OCR");
        }
    }
    let total = response.get("total").and_then(Value::as_u64).unwrap_or(0);
    let truncated = response
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if truncated {
        let shown = items.map(Vec::len).unwrap_or(0);
        println!("showing {shown} of {total} pending (use --limit / --offset)");
    } else {
        println!("total {total} pending");
    }
}
