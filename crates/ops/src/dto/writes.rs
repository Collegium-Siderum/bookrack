// SPDX-License-Identifier: Apache-2.0

//! Write-side request and response DTOs.
//!
//! Each [`writes`](crate::writes) op takes one of these structs as its
//! request and returns one as its response. Both surfaces — CLI and MCP
//! — build the request from their own argument parsing and serialize the
//! response.

use serde::{Deserialize, Serialize};

/// Request body for [`crate::writes::metadata::set_metadata_field`].
#[derive(Debug, Clone, Deserialize)]
pub struct SetMetadataFieldRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field to set (`title`, `publisher`, `year`, `language`, ...).
    pub field: String,
    /// The new value.
    pub value: String,
    /// Why this value is correct; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for [`crate::writes::metadata::clear_metadata_field`].
#[derive(Debug, Clone, Deserialize)]
pub struct ClearMetadataFieldRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field whose override should be removed.
    pub field: String,
    /// Why the override is being removed; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for [`crate::writes::metadata::void_metadata_field`].
#[derive(Debug, Clone, Deserialize)]
pub struct VoidMetadataFieldRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field whose extracted value should be suppressed.
    pub field: String,
    /// Why the extracted value is wrong; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for [`crate::writes::metadata::reaudit_metadata`].
#[derive(Debug, Clone, Deserialize)]
pub struct ReauditMetadataRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
}

/// What a re-audit computed and stored.
#[derive(Debug, Clone, Serialize)]
pub struct ReauditOutcome {
    /// The book that was re-audited.
    pub intake_id: i64,
    /// The stored verdict before this re-audit, if any.
    pub previous_verdict: Option<String>,
    /// The stored confidence before this re-audit, if any.
    pub previous_confidence: Option<String>,
    /// The verdict this re-audit computed and stored.
    pub verdict: String,
    /// The confidence this re-audit computed and stored.
    pub confidence: String,
}

/// Request body for [`crate::writes::metadata::acknowledge_metadata_gap`].
#[derive(Debug, Clone, Deserialize)]
pub struct AcknowledgeMetadataGapRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Why the gap is being acknowledged; recorded on the audit row.
    pub reason: String,
}

/// Request body for [`crate::writes::metadata::approve_metadata`].
#[derive(Debug, Clone, Deserialize)]
pub struct ApproveMetadataRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Optional reason recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for [`crate::writes::metadata::reject_metadata`].
#[derive(Debug, Clone, Deserialize)]
pub struct RejectMetadataRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Why the book is being rejected; recorded on the audit row.
    pub reason: String,
}

/// What a write op records about the change it just made.
///
/// Every write op returns one of these so the caller can render or log
/// the resulting audit identity without a second round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct WriteOutcome {
    /// Surrogate id of the `metadata_audit` row this op appended.
    pub audit_id: i64,
    /// Database string for the actor kind that performed the edit.
    pub actor_kind: String,
    /// Free-form actor identifier ("cli", "mcp", ...).
    pub actor_detail: Option<String>,
    /// True when the underlying state changed; false when the op was a
    /// no-op (e.g. clear with nothing to clear). The audit row is still
    /// written, so the trail records that someone tried.
    pub changed: bool,
}
