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
    /// Where the graded value comes from: `extracted`, `override`,
    /// `override_confirmed`, or `voided`.
    pub origin: String,
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
    /// per-field origins read off the override rows and the stored
    /// rollup and review status attached for comparison.
    pub fn build(
        intake_id: i64,
        profile: &str,
        report: &bookrack_ingest::MetadataReport,
        overrides: &[bookrack_catalog::NodeOverride],
        stored_verdict: Option<String>,
        stored_confidence: Option<String>,
        review_status: Option<String>,
    ) -> MetadataAuditReport {
        let origin_of = |field: &str| -> &'static str {
            match overrides.iter().find(|o| o.field == field) {
                None => "extracted",
                Some(over) if over.value.is_none() => "voided",
                Some(over) if over.confirmed => "override_confirmed",
                Some(_) => "override",
            }
        };
        MetadataAuditReport {
            intake_id,
            profile: profile.to_string(),
            fields: report
                .fields
                .iter()
                .map(|f| FieldAuditEntry {
                    field: f.field.clone(),
                    origin: origin_of(&f.field).to_string(),
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
    /// Best-effort title as reported elsewhere: the extracted value
    /// with the curator's corrections applied.
    pub title: Option<String>,
    /// The title as extraction and enrichment wrote it, before any
    /// correction. Equal to [`Self::title`] on a book no one has
    /// edited; `None` when the base layer never carried one.
    pub title_raw: Option<String>,
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

/// One graded field row of a [`PaperMetadataAuditReport`].
///
/// Peer of [`FieldAuditEntry`], not the same type: the paper audit
/// grades a different field set and its hint is optional, because a
/// clean field on that side has nothing to say.
#[derive(Debug, Clone, Serialize)]
pub struct PaperFieldAuditEntry {
    /// The effective-attrs field name being audited.
    pub field: String,
    /// Where the graded value comes from: `extracted`, `override`,
    /// `override_confirmed`, or `voided`.
    pub origin: String,
    /// The grade token: `missing` / `weak` / `medium` / `strong`.
    pub grade: String,
    /// Tokens of every flag that fired against the field.
    pub flags: Vec<String>,
    /// One short human-facing line, when the audit had one to give.
    pub hint: Option<String>,
}

/// The recomputed-audit read returned by
/// [`crate::reads::papers_metadata::show_paper_metadata_report`]: the
/// paper plausibility audit re-run from the cached extraction against
/// the current effective metadata, next to the judgement stored on the
/// audit projection row for comparison.
///
/// The two rollups disagreeing is the point of carrying both. It means
/// the paper has been edited since the stored judgement was made, and
/// `papers.metadata.reaudit` is what brings the row back in line.
#[derive(Debug, Clone, Serialize)]
pub struct PaperMetadataAuditReport {
    /// Catalog intake id of the paper.
    pub intake_id: i64,
    /// Name of the audit profile this recomputation ran under.
    pub profile: String,
    /// Per-field rows, in the audit's stable field order.
    pub fields: Vec<PaperFieldAuditEntry>,
    /// Tokens of the flags that belong to no single field.
    pub cross_field_flags: Vec<String>,
    /// The CSL type this recomputation selected its required-field
    /// matrix by. `None` when the profile disabled the audit.
    pub csl_type: Option<String>,
    /// The verdict this recomputation produced (`clean` /
    /// `needs_work`).
    pub verdict: String,
    /// The confidence this recomputation produced (`high` / `medium` /
    /// `low`).
    pub confidence: String,
    /// The verdict on the stored audit projection row, when one is
    /// recorded. This is what every other read surface reports.
    pub stored_verdict: Option<String>,
    /// The confidence on the stored row, when one is recorded.
    pub stored_confidence: Option<String>,
    /// When the stored judgement was made, ISO-8601 UTC.
    pub stored_audited_at: Option<String>,
    /// The profile the stored judgement ran under, which need not be
    /// the one this recomputation used.
    pub stored_profile_name: Option<String>,
    /// The current review status (`pending` / `acknowledged` /
    /// `approved` / `rejected`), when a review row exists.
    pub review_status: Option<String>,
}

impl PaperMetadataAuditReport {
    /// Project an in-memory paper audit report onto the wire shape,
    /// with the per-field origins read off the override rows and the
    /// stored judgement attached for comparison.
    #[allow(clippy::too_many_arguments)] // One argument per independent source the report is assembled from; a bundle struct would exist only to be destructured here.
    pub fn build(
        intake_id: i64,
        profile: &str,
        report: &bookrack_glean::audit::PaperReport,
        overrides: &[bookrack_catalog::NodeOverride],
        stored: Option<&bookrack_catalog::NodePaperAudit>,
        review_status: Option<String>,
    ) -> PaperMetadataAuditReport {
        let origin_of = |field: &str| -> &'static str {
            match overrides.iter().find(|o| o.field == field) {
                None => "extracted",
                Some(over) if over.value.is_none() => "voided",
                Some(over) if over.confirmed => "override_confirmed",
                Some(_) => "override",
            }
        };
        PaperMetadataAuditReport {
            intake_id,
            profile: profile.to_string(),
            fields: report
                .fields
                .iter()
                .map(|(name, f)| PaperFieldAuditEntry {
                    field: (*name).to_string(),
                    origin: origin_of(name).to_string(),
                    grade: f.grade.as_token().to_string(),
                    flags: f.flags.iter().map(|f| f.as_token().to_string()).collect(),
                    hint: f.hint.clone(),
                })
                .collect(),
            cross_field_flags: report
                .cross_field_flags
                .iter()
                .map(|f| f.as_token().to_string())
                .collect(),
            csl_type: report
                .csl_type
                .map(|t| bookrack_glean::audit::csl_type_token(t).to_string()),
            verdict: report.verdict.as_token().to_string(),
            confidence: report.confidence.as_token().to_string(),
            stored_verdict: stored.map(|s| s.verdict.clone()),
            stored_confidence: stored.map(|s| s.confidence.clone()),
            stored_audited_at: stored.map(|s| s.audited_at.clone()),
            stored_profile_name: stored.map(|s| s.profile_name.clone()),
            review_status,
        }
    }
}
