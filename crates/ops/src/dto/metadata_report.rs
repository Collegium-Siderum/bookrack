// SPDX-License-Identifier: Apache-2.0

//! The shapes of the metadata-status reads.
//!
//! [`MetadataReport`] is what `library.show_metadata_audit` returns: the
//! base [`BookDetail`] augmented with the persisted audit verdict and the
//! current review status, read straight from the catalog without touching
//! the extraction envelope.
//!
//! [`MetadataAuditReport`] is what `library.show_metadata_report` returns:
//! the plausibility audit recomputed from the cached extraction against
//! the current effective metadata, exposing the per-field grades, flags,
//! and hints next to the stored rollup for comparison.

use serde::Serialize;

use crate::dto::BookDetail;

/// The metadata-status read returned by
/// [`crate::reads::metadata::show_metadata_audit`].
#[derive(Debug, Clone, Serialize)]
pub struct MetadataReport {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Full bibliographic record for the book.
    pub book: BookDetail,
    /// The audit verdict the ingest pipeline stamped on the row, when
    /// one is recorded (`clean` / `needs_work` / ...).
    pub stored_verdict: Option<String>,
    /// The audit confidence stamped on the row (`high` / `medium` /
    /// `low`), when one is recorded.
    pub stored_confidence: Option<String>,
    /// The current review status (`pending` / `acknowledged` /
    /// `approved` / `rejected`), when a review row exists.
    pub review_status: Option<String>,
}

/// One graded field row of a [`MetadataAuditReport`].
#[derive(Debug, Clone, Serialize)]
pub struct FieldAuditEntry {
    /// The `node_publication_attrs` column name being audited.
    pub field: String,
    /// The grade token: `missing` / `weak` / `medium` / `strong`.
    pub grade: String,
    /// Tokens of every flag that fired against the field.
    pub flags: Vec<String>,
    /// One short human-facing line that summarises the row.
    pub hint: String,
}

/// The recomputed-audit read returned by
/// [`crate::reads::metadata::show_metadata_report`]: the plausibility
/// audit re-run from the book's cached extraction against the current
/// effective metadata, next to the stored rollup for comparison.
#[derive(Debug, Clone, Serialize)]
pub struct MetadataAuditReport {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Name of the audit profile the report was computed under.
    pub profile: String,
    /// Per-field rows, in the audit's stable display order.
    pub fields: Vec<FieldAuditEntry>,
    /// Tokens of the warning-level TOC shape flags, kept apart from the
    /// per-field rows: shape only pushes the verdict toward `needs_work`
    /// and the confidence toward `low`, never the other way.
    pub shape_flags: Vec<String>,
    /// The verdict this recomputation produced (`clean` / `needs_work`).
    pub verdict: String,
    /// The confidence this recomputation produced (`high` / `medium` /
    /// `low`).
    pub confidence: String,
    /// Block indices that may contain a copyright page — candidates for
    /// a cross-check against the source, not asserted matches.
    pub copyright_blocks: Vec<usize>,
    /// The audit verdict currently stored on the row, when one is
    /// recorded. May predate the recomputation or come from another
    /// profile.
    pub stored_verdict: Option<String>,
    /// The confidence currently stored on the row, when one is recorded.
    pub stored_confidence: Option<String>,
    /// The current review status (`pending` / `acknowledged` /
    /// `approved` / `rejected`), when a review row exists.
    pub review_status: Option<String>,
}

impl MetadataAuditReport {
    /// Project an in-memory audit report onto the wire shape, with the
    /// stored rollup and review status attached for comparison.
    pub fn build(
        intake_id: i64,
        profile: &str,
        report: &bookrack_ingest::MetadataReport,
        stored_verdict: Option<String>,
        stored_confidence: Option<String>,
        review_status: Option<String>,
    ) -> MetadataAuditReport {
        MetadataAuditReport {
            intake_id,
            profile: profile.to_string(),
            fields: report
                .fields
                .iter()
                .map(|f| FieldAuditEntry {
                    field: f.field.clone(),
                    grade: f.grade.as_str().to_string(),
                    flags: f
                        .flags
                        .iter()
                        .map(|flag| flag.token().to_string())
                        .collect(),
                    hint: f.hint.clone(),
                })
                .collect(),
            shape_flags: report
                .shape_flags
                .iter()
                .map(|flag| flag.token().to_string())
                .collect(),
            verdict: report.verdict.as_token().to_string(),
            confidence: report.confidence.as_str().to_string(),
            copyright_blocks: report.copyright_blocks.clone(),
            stored_verdict,
            stored_confidence,
            review_status,
        }
    }
}

/// One row of a paginated metadata list — returned by both
/// [`crate::reads::metadata::list_metadata`] (unfiltered) and
/// [`crate::reads::metadata::list_pending_reviews`] (review queue).
#[derive(Debug, Clone, Serialize)]
pub struct MetadataListRow {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Best-effort title.
    pub title: Option<String>,
    /// Confidence the audit assigned (`high` / `medium` / `low`).
    pub confidence: Option<String>,
    /// Current review status (`pending` / `acknowledged` / ...).
    pub review_status: Option<String>,
}

/// Paginated result of a metadata listing — see [`MetadataListRow`].
#[derive(Debug, Clone, Serialize)]
pub struct MetadataListPage {
    /// Books in this page.
    pub rows: Vec<MetadataListRow>,
    /// Total number of books matching the filter, regardless of
    /// pagination.
    pub total: u64,
    /// True when this page does not cover the full result set.
    pub truncated: bool,
}
