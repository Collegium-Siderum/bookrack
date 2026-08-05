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
    /// True when the curator has checked the value against the source
    /// itself. Recorded on the override row; the audit grades a
    /// confirmed override strong unless a validation check fails.
    #[serde(default)]
    pub confirmed: bool,
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

/// Request body for [`crate::writes::metadata::add_contributor`].
#[derive(Debug, Clone, Deserialize)]
pub struct AddContributorRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Contribution role; must be one of
    /// [`bookrack_catalog::CONTRIBUTOR_ROLES`].
    pub role: String,
    /// The contributor's name.
    pub name: String,
    /// The contributor's nationality, when known.
    #[serde(default)]
    pub nationality: Option<String>,
    /// Why this attribution is correct; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for [`crate::writes::metadata::remove_contributor`].
#[derive(Debug, Clone, Deserialize)]
pub struct RemoveContributorRequest {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Surrogate id of the contributor row, as listed by `show_book`.
    pub contributor_id: i64,
    /// Why the attribution is being removed; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// What [`crate::writes::metadata::add_contributor`] created.
#[derive(Debug, Clone, Serialize)]
pub struct AddContributorOutcome {
    /// Surrogate id of the new contributor row.
    pub contributor_id: i64,
    /// The audit identity of the write.
    #[serde(flatten)]
    pub write: WriteOutcome,
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

/// Request body for
/// [`crate::writes::papers_metadata::set_paper_metadata_field`].
#[derive(Debug, Clone, Deserialize)]
pub struct PaperSetMetadataFieldRequest {
    /// Catalog intake id of the paper.
    pub intake_id: i64,
    /// The field to set; must be one of
    /// [`crate::writes::papers_metadata::PAPER_EDITABLE_FIELDS`].
    pub field: String,
    /// The new value.
    pub value: String,
    /// Why this value is correct; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// True when the curator has checked the value against the source
    /// itself. Recorded on the override row; the audit grades a
    /// confirmed override strong unless a validation check fails.
    #[serde(default)]
    pub confirmed: bool,
}

/// Request body for
/// [`crate::writes::papers_metadata::clear_paper_metadata_field`].
#[derive(Debug, Clone, Deserialize)]
pub struct PaperClearMetadataFieldRequest {
    /// Catalog intake id of the paper.
    pub intake_id: i64,
    /// The field whose override should be removed.
    pub field: String,
    /// Why the override is being removed; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for
/// [`crate::writes::papers_metadata::void_paper_metadata_field`].
#[derive(Debug, Clone, Deserialize)]
pub struct PaperVoidMetadataFieldRequest {
    /// Catalog intake id of the paper.
    pub intake_id: i64,
    /// The field whose extracted value should be suppressed.
    pub field: String,
    /// Why the extracted value is wrong; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for
/// [`crate::writes::papers_metadata::add_paper_contributor`].
///
/// Carries the structured name parts and the ORCID a citation needs,
/// and no nationality: that one is a book-side enrichment field.
#[derive(Debug, Clone, Deserialize)]
pub struct PaperContributorAddRequest {
    /// Catalog intake id of the paper.
    pub intake_id: i64,
    /// Contribution role; must be one of
    /// [`bookrack_catalog::CONTRIBUTOR_ROLES`].
    pub role: String,
    /// The contributor's name as it should be displayed.
    pub name: String,
    /// Family name, when the parts are known separately.
    #[serde(default)]
    pub family: Option<String>,
    /// Given name, when the parts are known separately.
    #[serde(default)]
    pub given: Option<String>,
    /// The contributor's ORCID, when known.
    #[serde(default)]
    pub orcid: Option<String>,
    /// Why this attribution is correct; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for
/// [`crate::writes::papers_metadata::remove_paper_contributor`].
#[derive(Debug, Clone, Deserialize)]
pub struct PaperContributorRemoveRequest {
    /// Catalog intake id of the paper the row belongs to. The
    /// surrogate id alone addresses a row anywhere in the catalog, so
    /// this is what makes the request checkable.
    pub intake_id: i64,
    /// Surrogate id of the contributor row, as listed by `show_paper`.
    pub contributor_id: i64,
    /// Why the attribution is being removed; recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for the four paper review-status transitions
/// (`ack` / `approve` / `reject` / `reopen`), which differ only in the
/// status and audit action their op supplies.
#[derive(Debug, Clone, Deserialize)]
pub struct PaperReviewRequest {
    /// Catalog intake id of the paper.
    pub intake_id: i64,
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
