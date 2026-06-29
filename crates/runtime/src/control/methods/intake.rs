// SPDX-License-Identifier: Apache-2.0

//! `intake.ocr` JSON-RPC handler.
//!
//! Treats an OCR markdown product + its source scan PDF as one
//! queue-bound book ingest job: the handler only mutates the on-disk
//! queue document, the worker runs the actual pipeline. The job rides
//! the book queue with `kind == ItemKind::Book` and an
//! [`IntakeOcrInfo`] sidecar so a `queue.list` reads it as a book job
//! while the worker routes on the sidecar.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use ts_rs::TS;

use super::MethodContext;
use super::ingest::{PriorityRepr, derive_tick};
use crate::control::events::Event;
use crate::control::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, RpcError};
use crate::queue::{self, IntakeOcrInfo};

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct IntakeOcrParams {
    /// Path to the OCR markdown product, with page markers
    /// `<!-- page <label> (sheet <n>) -->`.
    #[cfg_attr(test, ts(type = "string"))]
    ocr_md: PathBuf,
    /// Path to the scan PDF the OCR product was produced from.
    #[cfg_attr(test, ts(type = "string"))]
    from_pdf: PathBuf,
    /// Override the expected page count rather than reading it from
    /// the source PDF's `/Pages`.
    #[serde(default)]
    expected_pages: Option<u32>,
    /// Accept a partial OCR product. The present sheets are recorded
    /// into `Provenance.partial_pages`; missing pages surface in the
    /// OCR intake's `partial_pages` field rather than being silently
    /// treated as blank.
    #[serde(default)]
    allow_partial: bool,
    #[serde(default)]
    library: Option<String>,
    #[serde(default)]
    priority: Option<PriorityRepr>,
    #[serde(default)]
    force: bool,
    /// When `true`, the worker parks the resulting book at STRUCTURE
    /// if the audit verdict is `needs_work`, skipping CHUNK and EMBED
    /// until a curator drives it past the metadata gate. Mirrors the
    /// `ingest.submit` flag of the same name.
    #[serde(default)]
    hold_for_metadata: bool,
    /// Optional book-side audit profile name applied to the OCR
    /// intake's book pipeline. Resolves through the same built-in set
    /// as `ingest.submit`; absent means the daemon's startup profile.
    #[serde(default)]
    audit_profile: Option<String>,
}

pub async fn submit(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: IntakeOcrParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid intake.ocr params: {e}"))
        })?,
        _ => {
            return Err(RpcError::new(INVALID_PARAMS, "missing intake.ocr params"));
        }
    };
    if !parsed.ocr_md.is_file() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            format!("ocr_md: not a regular file: {}", parsed.ocr_md.display()),
        ));
    }
    if !parsed.from_pdf.is_file() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            format!(
                "from_pdf: not a regular file: {}",
                parsed.from_pdf.display()
            ),
        ));
    }
    let library = parsed.library.unwrap_or_else(|| ctx.library_name.clone());
    let priority = parsed
        .priority
        .map(PriorityRepr::into_priority)
        .unwrap_or_default();
    let info = IntakeOcrInfo {
        from_pdf: parsed.from_pdf,
        expected_pages: parsed.expected_pages,
        allow_partial: parsed.allow_partial,
    };
    let id = {
        let mut guard = ctx
            .queue_state
            .lock()
            .map_err(|_| RpcError::new(INTERNAL_ERROR, "queue state lock poisoned"))?;
        let id = queue::enqueue_ocr_intake(
            &mut guard,
            parsed.ocr_md,
            info,
            &library,
            priority,
            parsed.force,
            parsed.hold_for_metadata,
            parsed.audit_profile.clone(),
        );
        queue::save_atomic(&guard, &ctx.queue_state_path)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("persist queue state: {e}")))?;
        let tick = derive_tick(&guard, None);
        ctx.event_stream.publish(Event::QueueTick(tick));
        id
    };
    Ok(json!({ "job_id": id }))
}
