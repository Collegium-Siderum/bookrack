// SPDX-License-Identifier: Apache-2.0

//! Read ops over the paper-side metadata audit and its edit trail.
//!
//! Peer of [`crate::reads::metadata`] for the papers pipeline. The
//! trail read shares that module's body — `metadata_audit` carries
//! nothing pipeline-specific, and each pipeline's rows live in its own
//! catalog — while the per-field report is its own function, because
//! the two pipelines audit different fields and their report types
//! have no common shape.

use bookrack_catalog::{Catalog, IntakeFilter};
use bookrack_core::ItemKind;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::MetadataFilter;
use crate::dto::audit::AuditTrailEntry;
use crate::dto::metadata_report::{MetadataListPage, PaperMetadataAuditReport};
use crate::reads::metadata::{list_metadata_inner, needs_review_filter, show_audit_trail_inner};
use crate::recorder::record_call_sync;

/// Recompute the metadata plausibility audit for one paper from its
/// cached extraction envelope and return the full per-field report —
/// grades, flags, and hints — next to the judgement stored on the
/// audit projection row.
///
/// Pure read: nothing is written back. The recomputation and the
/// stored judgement disagreeing means the paper has been edited since
/// that judgement was made, and `papers.metadata.reaudit` is the write
/// path that brings the row back in line.
pub fn show_paper_metadata_report<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
    audit_data: &bookrack_glean::audit::PaperAuditData,
    audit_profile: &bookrack_glean::audit::PaperAuditProfile,
) -> Result<PaperMetadataAuditReport> {
    record_call_sync!(
        ops,
        "library.show_paper_metadata_report",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let papers_db = ops
                .papers_catalog_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let catalog = Catalog::open_read_only(papers_db)?;
            let report = bookrack_glean::reaudit::build_report(
                &catalog,
                intake_id,
                audit_profile,
                audit_data,
            )
            .map_err(|e| match e {
                bookrack_glean::GleanError::UnknownIntake(intake_id) => {
                    OpsError::IntakeNotFound { intake_id }
                }
                other => OpsError::Other(eyre::Report::new(other)),
            })?;
            let overrides = catalog.overrides_for_address(intake_id, ItemKind::Paper)?;
            let stored = catalog.node_paper_audit(intake_id, ItemKind::Paper.as_scope_str())?;
            let review_status = catalog
                .review(intake_id, ItemKind::Paper)?
                .map(|r| r.status);
            Ok(PaperMetadataAuditReport::build(
                intake_id,
                &audit_profile.name,
                &report,
                &overrides,
                stored.as_ref(),
                review_status,
            ))
        }
    )
}

/// Read the metadata-edit audit trail for one paper, oldest first.
///
/// `metadata_audit` rows outlive the paper they describe: `papers
/// remove` drops the intake row and preserves the history. Rows are
/// therefore returned whenever any exist, whether or not the
/// `intake_id` is still registered; only no rows *and* no intake is
/// [`OpsError::IntakeNotFound`] — the "ghost id" case.
pub fn show_paper_audit_trail<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
) -> Result<Vec<AuditTrailEntry>> {
    record_call_sync!(
        ops,
        "library.show_paper_audit_trail",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let papers_db = ops
                .papers_catalog_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            show_audit_trail_inner(papers_db, intake_id)
        }
    )
}

/// List papers with their current confidence and review status,
/// narrowed by `filter`. Paginated.
///
/// The title predicate reads the base layer — what extraction and
/// enrichment wrote — so a row reached by its extracted title is the
/// one a review pass is looking for. Each row carries both layers.
/// To search the reported titles, use
/// [`reads::papers::find_papers`](crate::reads::papers::find_papers).
pub fn list_paper_metadata<E: Embedder>(
    ops: &Ops<E>,
    filter: MetadataFilter,
    limit: u32,
    offset: u32,
) -> Result<MetadataListPage> {
    record_call_sync!(
        ops,
        "library.list_paper_metadata",
        serde_json::json!({
            "title_substring": filter.title_substring,
            "confidence_in": filter.confidence_in,
            "review_status_in": filter.review_status_in,
            "limit": limit,
            "offset": offset,
        }),
        {
            let papers_db = ops
                .papers_catalog_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let confidence_in: Vec<&str> =
                filter.confidence_in.iter().map(String::as_str).collect();
            let review_status_in: Vec<&str> =
                filter.review_status_in.iter().map(String::as_str).collect();
            let catalog_filter = IntakeFilter {
                title_substring: filter.title_substring.as_deref(),
                confidence_in: confidence_in.as_slice(),
                review_status_in: review_status_in.as_slice(),
                ..IntakeFilter::default()
            };
            list_metadata_inner(papers_db, ItemKind::Paper, catalog_filter, limit, offset)
        }
    )
}

/// List papers still on the review queue: low / medium confidence plus
/// pending / acknowledged review status. Paginated.
///
/// A preset over the same listing [`list_paper_metadata`] serves, on
/// the book side's filter rather than a paper-specific one: the
/// confidence and review vocabularies are one set across the two
/// pipelines.
pub fn list_paper_pending_reviews<E: Embedder>(
    ops: &Ops<E>,
    limit: u32,
    offset: u32,
) -> Result<MetadataListPage> {
    record_call_sync!(
        ops,
        "library.list_paper_pending_reviews",
        serde_json::json!({ "limit": limit, "offset": offset }),
        {
            let papers_db = ops
                .papers_catalog_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            list_metadata_inner(
                papers_db,
                ItemKind::Paper,
                needs_review_filter(),
                limit,
                offset,
            )
        }
    )
}
