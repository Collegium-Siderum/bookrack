// SPDX-License-Identifier: Apache-2.0

//! Caller identity and audit-trail entries.
//!
//! A [`Caller`] is the (kind, detail, session, reason) bundle every write
//! op stamps onto its `metadata_audit` row. [`AuditTrailEntry`] and
//! [`PipelineAuditEntry`] are the read-side projections of those rows.

use serde::Serialize;

use bookrack_catalog::{ActorKind, ItemPipelineAudit, MetadataAudit};

/// Who (or what) initiated an op, with the optional bookkeeping every
/// write op forwards to `metadata_audit`. CLI builds this with
/// [`crate::Caller::cli`]; the MCP server builds it with
/// [`crate::Caller::mcp`].
#[derive(Debug, Clone)]
pub struct Caller {
    /// The closed-set kind enforced by the audit-table `CHECK` constraint.
    pub actor_kind: ActorKind,
    /// Free-form identifier of the surface ("cli", "mcp", or a richer
    /// label set by the caller).
    pub actor_detail: Option<String>,
    /// Session id for grouping a sequence of edits.
    pub session_id: Option<String>,
    /// Optional human-readable reason carried on every write this caller
    /// makes; individual ops can override it.
    pub reason: Option<String>,
}

/// One row of the metadata-edit audit trail, projected for the wire.
///
/// Decoupled from [`bookrack_catalog::MetadataAudit`] so a catalog schema
/// change does not break MCP clients. The `actor_kind` field carries the
/// database string ("human", "llm", ...), not the enum variant name.
#[derive(Debug, Clone, Serialize)]
pub struct AuditTrailEntry {
    /// Surrogate key assigned by the database.
    pub audit_id: i64,
    /// The edited node — a soft reference; `None` for non-node edits.
    pub node_id: Option<i64>,
    /// The table the edit touched.
    pub table_name: String,
    /// The edited field; `None` for a row-level insert or delete.
    pub field: Option<String>,
    /// What happened (`insert` / `update` / `delete` / ...).
    pub action: String,
    /// The value before the edit.
    pub old_value: Option<String>,
    /// The value after the edit.
    pub new_value: Option<String>,
    /// When the edit was made, ISO-8601 UTC.
    pub changed_at: String,
    /// Kind of actor (database string: `human` / `llm` / `import` /
    /// `pipeline` / `system`).
    pub actor_kind: String,
    /// Free-form actor identifier (e.g. "cli", "mcp").
    pub actor_detail: Option<String>,
    /// Optional reason recorded with the edit.
    pub reason: Option<String>,
    /// Session id this edit belongs to.
    pub session_id: Option<String>,
}

impl AuditTrailEntry {
    /// Project a catalog [`MetadataAudit`] row into a wire-ready entry.
    pub fn from_row(row: MetadataAudit) -> AuditTrailEntry {
        AuditTrailEntry {
            audit_id: row.audit_id,
            node_id: row.node_id,
            table_name: row.table_name,
            field: row.field,
            action: row.action,
            old_value: row.old_value,
            new_value: row.new_value,
            changed_at: row.changed_at,
            actor_kind: row.actor_kind.as_str().to_string(),
            actor_detail: row.actor_detail,
            reason: row.reason,
            session_id: row.session_id,
        }
    }
}

/// One row of the book-level pipeline audit trail, projected for the wire.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineAuditEntry {
    /// Surrogate key assigned by the database.
    pub audit_id: i64,
    /// The book root this entry concerns; `None` when not tied to a book.
    pub book_root_id: Option<i64>,
    /// Pipeline stage (`extract`, `structure`, `metadata`, ...).
    pub stage: String,
    /// Sub-step within the stage.
    pub sub_step: String,
    /// Outcome (`ok` / `fail` / `partial` / `skipped`).
    pub outcome: String,
    /// Unique id tying every sub-step of one pipeline run together.
    pub pipeline_run_id: String,
    /// Kind of actor (database string).
    pub actor_kind: String,
    /// Free-form actor identifier.
    pub actor_detail: Option<String>,
    /// The adapter that ran the sub-step, when one applies.
    pub adapter: Option<String>,
    /// Source-file sha256 the run consumed, when applicable.
    pub source_sha256: Option<String>,
    /// Compact metric summary (JSON-encoded).
    pub metric_summary: Option<String>,
    /// Free-form error message if `outcome != ok`.
    pub error_message: Option<String>,
    /// Wall-clock duration of the step in milliseconds, when measured.
    pub duration_ms: Option<i64>,
    /// When the entry was written, ISO-8601 UTC.
    pub ts: String,
    /// The session the run belongs to.
    pub session_id: Option<String>,
}

impl PipelineAuditEntry {
    /// Project a catalog [`ItemPipelineAudit`] row into a wire-ready entry.
    pub fn from_row(row: ItemPipelineAudit) -> PipelineAuditEntry {
        PipelineAuditEntry {
            audit_id: row.audit_id,
            book_root_id: row.book_root_id,
            stage: row.stage,
            sub_step: row.sub_step,
            outcome: row.outcome,
            pipeline_run_id: row.pipeline_run_id,
            actor_kind: row.actor_kind.as_str().to_string(),
            actor_detail: row.actor_detail,
            adapter: row.adapter,
            source_sha256: row.source_sha256,
            metric_summary: row.metric_summary,
            error_message: row.error_message,
            duration_ms: row.duration_ms,
            ts: row.ts,
            session_id: row.session_id,
        }
    }
}
