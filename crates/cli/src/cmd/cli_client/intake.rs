// SPDX-License-Identifier: Apache-2.0

//! `bookrack intake` — one-shot control-plane client for derived-source
//! intake commands. Currently only `intake ocr`, which enqueues an OCR
//! markdown product against its scan PDF anchor onto the persistent
//! ingest queue. The daemon runs the actual pipeline; this client
//! follows worker progress until the job leaves the queue.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::IntakeAction;
use serde_json::{Value, json};

use super::helpers;

pub async fn run(action: IntakeAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    match action {
        IntakeAction::Ocr {
            ocr_md,
            from_pdf,
            expected_pages,
            allow_partial,
        } => {
            let mut params = json!({
                "ocr_md": ocr_md,
                "from_pdf": from_pdf,
                "allow_partial": allow_partial,
            });
            if let Some(pages) = expected_pages {
                params["expected_pages"] = Value::from(pages);
            }
            helpers::call_with_progress(client, "intake.ocr", params).await
        }
    }
}
