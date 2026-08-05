// SPDX-License-Identifier: Apache-2.0

//! Read ops over the paper-side metadata audit and its edit trail.
//!
//! Peer of [`crate::reads::metadata`] for the papers pipeline. The
//! trail read shares that module's body — `metadata_audit` carries
//! nothing pipeline-specific, and each pipeline's rows live in its own
//! catalog — while the per-field report is its own function, because
//! the two pipelines audit different fields and their report types
//! have no common shape.

use bookrack_catalog::Catalog;
use bookrack_core::ItemKind;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::audit::AuditTrailEntry;
use crate::dto::metadata_report::PaperMetadataAuditReport;
use crate::reads::metadata::show_audit_trail_inner;
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
