// SPDX-License-Identifier: Apache-2.0

//! Catalog snapshots: head of the `intake` table plus the recent
//! windows of three observability tables.

use std::path::Path;

use bookrack_catalog::Catalog;
use bookrack_config::Config;
use serde::Serialize;

use crate::Result;
use crate::scrub::Scrubber;

/// How many recent rows of each observability table to include. Larger
/// than the default `--days` window of 7 is fine: we want some context
/// past the cutoff if the table happens to be sparse.
const RECENT_ROW_CAP: u32 = 1000;
/// How many intake rows to capture at the head of the table.
const INTAKE_HEAD_CAP: u32 = 50;

/// Write `<bundle>/catalog/{intakes-head,tool-calls,pipeline-audit,
/// metadata-audit}.json`. A catalog that fails to open is reported as
/// an error; missing or empty tables write an empty JSON array.
pub fn collect(cfg: &Config, since_ts: &str, bundle_dir: &Path, scrubber: &Scrubber) -> Result<()> {
    let dst = bundle_dir.join("catalog");
    std::fs::create_dir_all(&dst)?;

    let catalog = match Catalog::open_read_only(&cfg.catalog_db()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "diagnose: could not open catalog read-only");
            return Ok(());
        }
    };

    let intakes = catalog.intakes_head(INTAKE_HEAD_CAP)?;
    write_json(
        &dst.join("intakes-head.json"),
        intakes_to_json(&intakes),
        scrubber,
    )?;

    let tool_calls = catalog.recent_tool_calls(since_ts, RECENT_ROW_CAP)?;
    write_json(
        &dst.join("tool-calls.json"),
        tool_calls_to_json(&tool_calls),
        scrubber,
    )?;

    let pipeline = catalog.recent_pipeline_audit(since_ts, RECENT_ROW_CAP)?;
    write_json(
        &dst.join("pipeline-audit.json"),
        pipeline_to_json(&pipeline),
        scrubber,
    )?;

    let metadata = catalog.recent_metadata_audit(since_ts, RECENT_ROW_CAP)?;
    write_json(
        &dst.join("metadata-audit.json"),
        metadata_to_json(&metadata),
        scrubber,
    )?;

    Ok(())
}

fn write_json(path: &Path, value: serde_json::Value, scrubber: &Scrubber) -> Result<()> {
    let mut v = value;
    scrubber.scrub_value(&mut v);
    let mut text = serde_json::to_string_pretty(&v)?;
    text.push('\n');
    std::fs::write(path, text)?;
    Ok(())
}

/// Project an [`bookrack_catalog::Intake`] row into a compact JSON
/// shape: the fields a maintainer actually reads when triaging.
#[derive(Serialize)]
struct IntakeHead<'a> {
    intake_id: i64,
    format: Option<&'a str>,
    adapter: Option<&'a str>,
    extractor_version: u32,
    status: &'a str,
    page_count: Option<i64>,
    source_sha256: &'a str,
    original_path: Option<&'a str>,
    intake_at: &'a str,
}

fn intakes_to_json(rows: &[bookrack_catalog::Intake]) -> serde_json::Value {
    let projected: Vec<IntakeHead<'_>> = rows
        .iter()
        .map(|r| IntakeHead {
            intake_id: r.intake_id,
            format: r.format.as_deref(),
            adapter: r.adapter.as_deref(),
            extractor_version: r.extractor_version,
            status: r.status.as_str(),
            page_count: r.page_count,
            source_sha256: &r.source_sha256,
            original_path: r.original_path.as_deref(),
            intake_at: &r.intake_at,
        })
        .collect();
    serde_json::to_value(&projected).unwrap_or(serde_json::Value::Null)
}

#[derive(Serialize)]
struct ToolCallRow<'a> {
    call_id: i64,
    ts: &'a str,
    source: &'a str,
    tool: &'a str,
    status: &'a str,
    duration_ms: Option<f64>,
    error_type: Option<&'a str>,
    error_msg: Option<&'a str>,
    args: Option<&'a str>,
}

fn tool_calls_to_json(rows: &[bookrack_catalog::McpToolCall]) -> serde_json::Value {
    let projected: Vec<ToolCallRow<'_>> = rows
        .iter()
        .map(|r| ToolCallRow {
            call_id: r.call_id,
            ts: &r.ts,
            source: &r.source,
            tool: &r.tool,
            status: &r.status,
            duration_ms: r.duration_ms,
            error_type: r.error_type.as_deref(),
            error_msg: r.error_msg.as_deref(),
            args: r.args.as_deref(),
        })
        .collect();
    serde_json::to_value(&projected).unwrap_or(serde_json::Value::Null)
}

#[derive(Serialize)]
struct PipelineRow<'a> {
    audit_id: i64,
    ts: &'a str,
    stage: &'a str,
    sub_step: &'a str,
    outcome: &'a str,
    pipeline_run_id: &'a str,
    book_root_id: Option<i64>,
    adapter: Option<&'a str>,
    metric_summary: Option<&'a str>,
    error_message: Option<&'a str>,
    duration_ms: Option<i64>,
}

fn pipeline_to_json(rows: &[bookrack_catalog::BookPipelineAudit]) -> serde_json::Value {
    let projected: Vec<PipelineRow<'_>> = rows
        .iter()
        .map(|r| PipelineRow {
            audit_id: r.audit_id,
            ts: &r.ts,
            stage: &r.stage,
            sub_step: &r.sub_step,
            outcome: &r.outcome,
            pipeline_run_id: &r.pipeline_run_id,
            book_root_id: r.book_root_id,
            adapter: r.adapter.as_deref(),
            metric_summary: r.metric_summary.as_deref(),
            error_message: r.error_message.as_deref(),
            duration_ms: r.duration_ms,
        })
        .collect();
    serde_json::to_value(&projected).unwrap_or(serde_json::Value::Null)
}

#[derive(Serialize)]
struct MetadataRow<'a> {
    audit_id: i64,
    changed_at: &'a str,
    table_name: &'a str,
    action: &'a str,
    field: Option<&'a str>,
    old_value: Option<&'a str>,
    new_value: Option<&'a str>,
    node_id: Option<i64>,
    actor_kind: &'a str,
    actor_detail: Option<&'a str>,
    reason: Option<&'a str>,
}

fn metadata_to_json(rows: &[bookrack_catalog::MetadataAudit]) -> serde_json::Value {
    let projected: Vec<MetadataRow<'_>> = rows
        .iter()
        .map(|r| MetadataRow {
            audit_id: r.audit_id,
            changed_at: &r.changed_at,
            table_name: &r.table_name,
            action: &r.action,
            field: r.field.as_deref(),
            old_value: r.old_value.as_deref(),
            new_value: r.new_value.as_deref(),
            node_id: r.node_id,
            actor_kind: r.actor_kind.as_str(),
            actor_detail: r.actor_detail.as_deref(),
            reason: r.reason.as_deref(),
        })
        .collect();
    serde_json::to_value(&projected).unwrap_or(serde_json::Value::Null)
}
