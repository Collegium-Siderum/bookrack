// SPDX-License-Identifier: Apache-2.0

//! Read ops over the metadata audit trail and the review queue.

use bookrack_catalog::{Catalog, IntakeFilter, STATUS_ACKNOWLEDGED, STATUS_PENDING};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::audit::AuditTrailEntry;
use crate::dto::metadata_report::{
    MetadataAuditReport, MetadataListPage, MetadataListRow, MetadataReport,
};
use crate::dto::{BookDetail, MetadataFilter, TocStats, clamp_limit};
use crate::recorder::record_call_sync;

/// Read the metadata-status record for one book: bibliographic detail
/// plus the persisted audit verdict, confidence, and review status.
pub fn show_metadata_audit<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<MetadataReport> {
    record_call_sync!(
        ops,
        "library.show_metadata_audit",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let catalog = Catalog::open_read_only(ops.catalog_db())?;
            let Some(intake) = catalog.intake_by_id(intake_id)? else {
                return Err(OpsError::IntakeNotFound { intake_id });
            };
            let effective =
                catalog.effective_publication_attrs(intake.intake_id, ItemKind::Book)?;
            let overrides = catalog.overrides_for_address(intake.intake_id, ItemKind::Book)?;
            let contributors =
                catalog.contributors_for_address(intake.intake_id, ItemKind::Book)?;
            let attrs = catalog.publication_attrs(intake.intake_id, ItemKind::Book)?;
            let review_status = catalog
                .review(intake.intake_id, ItemKind::Book)?
                .map(|r| r.status);
            let stored_verdict = attrs.as_ref().and_then(|a| a.audit_verdict.clone());
            let stored_confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
            let corpus = Corpus::open_read_only(ops.corpus_db())?;
            let toc_stats = corpus
                .toc_stats_for_book(PartitionIdx::new(intake_id).root())?
                .map(TocStats::from);
            let book = BookDetail::build(intake, effective, overrides, contributors, toc_stats);
            Ok(MetadataReport {
                intake_id,
                book,
                stored_verdict,
                stored_confidence,
                review_status,
            })
        }
    )
}

/// Recompute the metadata plausibility audit for one book from its
/// cached extraction envelope and return the full per-field report —
/// grades, flags, and hints plus the TOC shape flags — next to the
/// stored rollup for comparison. Pure read: nothing is written back;
/// `metadata.reaudit` is the write path that refreshes the rollup.
pub fn show_metadata_report<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
    audit_data: &bookrack_ingest::AuditData,
    audit_profile: &bookrack_ingest::AuditProfile,
) -> Result<MetadataAuditReport> {
    record_call_sync!(
        ops,
        "library.show_metadata_report",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let catalog = Catalog::open_read_only(ops.catalog_db())?;
            let report = bookrack_ingest::reaudit::build_report(
                &catalog,
                intake_id,
                audit_data,
                audit_profile,
            )
            .map_err(|e| match e {
                bookrack_ingest::IngestError::UnknownIntake(intake_id) => {
                    OpsError::IntakeNotFound { intake_id }
                }
                other => OpsError::Other(eyre::Report::new(other)),
            })?;
            let overrides = catalog.overrides_for_address(intake_id, ItemKind::Book)?;
            let attrs = catalog.publication_attrs(intake_id, ItemKind::Book)?;
            let review_status = catalog.review(intake_id, ItemKind::Book)?.map(|r| r.status);
            Ok(MetadataAuditReport::build(
                intake_id,
                &audit_profile.name,
                &report,
                &overrides,
                attrs.as_ref().and_then(|a| a.audit_verdict.clone()),
                attrs.as_ref().and_then(|a| a.confidence.clone()),
                review_status,
            ))
        }
    )
}

/// The confidence grades an item on the review queue carries.
const NEEDS_REVIEW_CONFIDENCE: &[&str] = &["low", "medium"];

/// The review states an item on the review queue carries. An item
/// never reviewed counts as `pending`.
const NEEDS_REVIEW_STATUS: &[&str] = &[STATUS_PENDING, STATUS_ACKNOWLEDGED];

/// The filter behind the review-queue preset, shared by both
/// pipelines: the confidence and review vocabularies are one set, so a
/// second threshold on the paper side would be a second answer to the
/// same question.
pub(crate) fn needs_review_filter() -> IntakeFilter<'static> {
    IntakeFilter {
        confidence_in: NEEDS_REVIEW_CONFIDENCE,
        review_status_in: NEEDS_REVIEW_STATUS,
        ..IntakeFilter::default()
    }
}

/// List registered books with their current confidence and review
/// status, narrowed by `filter`. Paginated.
///
/// The title predicate reads the base layer — what extraction and
/// enrichment wrote — so a row reached by its extracted title is the
/// one a review pass is looking for. Each row carries both layers:
/// [`MetadataListRow::title_raw`] as extracted, and
/// [`MetadataListRow::title`] as reported everywhere else. To search
/// the reported titles, use
/// [`reads::books::find_books`](crate::reads::books::find_books).
pub fn list_metadata<E: Embedder>(
    ops: &Ops<E>,
    filter: MetadataFilter,
    limit: u32,
    offset: u32,
) -> Result<MetadataListPage> {
    record_call_sync!(
        ops,
        "library.list_metadata",
        serde_json::json!({
            "title_substring": filter.title_substring,
            "confidence_in": filter.confidence_in,
            "review_status_in": filter.review_status_in,
            "limit": limit,
            "offset": offset,
        }),
        {
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
            list_metadata_inner(
                &Catalog::open_read_only(ops.catalog_db())?,
                ItemKind::Book,
                catalog_filter,
                limit,
                offset,
            )
        }
    )
}

/// List books still on the review queue: low / medium confidence plus
/// pending / acknowledged review status. Paginated.
///
/// A preset over the same listing [`list_metadata`] serves: it is the
/// one question asked often enough to earn its own verb, and it shares
/// that function's filter shape rather than a second query.
pub fn list_pending_reviews<E: Embedder>(
    ops: &Ops<E>,
    limit: u32,
    offset: u32,
) -> Result<MetadataListPage> {
    record_call_sync!(
        ops,
        "library.list_pending_reviews",
        serde_json::json!({ "limit": limit, "offset": offset }),
        {
            list_metadata_inner(
                &Catalog::open_read_only(ops.catalog_db())?,
                ItemKind::Book,
                needs_review_filter(),
                limit,
                offset,
            )
        }
    )
}

/// Shared body of the paginated metadata listings, over whichever
/// catalog handle and item kind the caller passes. Pulled out so the
/// public entry points stay thin and the filter shape and the pipeline
/// are the only things that differ between them.
///
/// The row shape carries nothing pipeline-specific, so one body serves
/// both sides; what a caller must get right is pairing the catalog
/// with the kind stored in it.
pub(crate) fn list_metadata_inner(
    catalog: &Catalog,
    kind: ItemKind,
    filter: IntakeFilter<'_>,
    limit: u32,
    offset: u32,
) -> Result<MetadataListPage> {
    let (effective_limit, _) = clamp_limit(limit);
    // The kind reaches the query twice: `IntakeFilter::kind` decides
    // which scope every `node_*` JOIN picks up, and the projection
    // reads below take it directly. Setting it here rather than
    // trusting the caller's filter keeps those two from disagreeing —
    // a filter left on the default scope returns the other pipeline's
    // rows against this pipeline's catalog, which is an empty page
    // rather than an error.
    let filter = IntakeFilter { kind, ..filter };
    let (intakes, total) = catalog.find_intakes_page(&filter, effective_limit, offset)?;
    let intake_ids: Vec<i64> = intakes.iter().map(|i| i.intake_id).collect();
    let effective = catalog.effective_publication_attrs_for_intakes(&intake_ids, kind)?;
    let attrs = catalog.publication_attrs_for_intakes(&intake_ids, kind)?;
    let reviews = catalog.reviews_for_addresses(&intake_ids, kind)?;
    let rows: Vec<MetadataListRow> = intakes
        .iter()
        .map(|intake| {
            let title = effective
                .get(&intake.intake_id)
                .and_then(|e| e.get("title").map(str::to_string));
            let title_raw = attrs.get(&intake.intake_id).and_then(|a| a.title.clone());
            let confidence = attrs
                .get(&intake.intake_id)
                .and_then(|a| a.confidence.clone());
            let review_status = reviews.get(&intake.intake_id).map(|r| r.status.clone());
            MetadataListRow {
                intake_id: intake.intake_id,
                title,
                title_raw,
                confidence,
                review_status,
            }
        })
        .collect();
    let returned = rows.len() as u64;
    let truncated = u64::from(offset) + returned < total;
    Ok(MetadataListPage {
        rows,
        total,
        truncated,
    })
}

/// Read the metadata-edit audit trail for one book, oldest first.
///
/// `metadata_audit` rows outlive their book by design: `bookrack
/// remove` drops the `intake` row but preserves the audit history.
/// This read therefore surfaces rows whenever any exist, regardless of
/// whether the `intake_id` is still registered. Only when no rows
/// exist AND no `intake` is registered for the id is it reported as
/// [`OpsError::IntakeNotFound`] — that is the "ghost id" case.
pub fn show_audit_trail<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<Vec<AuditTrailEntry>> {
    record_call_sync!(
        ops,
        "library.show_audit_trail",
        serde_json::json!({ "intake_id": intake_id }),
        { show_audit_trail_inner(&Catalog::open_read_only(ops.catalog_db())?, intake_id) }
    )
}

/// Shared body of the audit-trail read, over whichever catalog handle
/// the caller passes.
///
/// `metadata_audit` has no scope column — each pipeline's rows live in
/// its own catalog — so the catalog handle is the whole of what differs
/// between the two sides.
pub(crate) fn show_audit_trail_inner(
    catalog: &Catalog,
    intake_id: i64,
) -> Result<Vec<AuditTrailEntry>> {
    let node_id = PartitionIdx::new(intake_id).root().get();
    let rows = catalog.metadata_audit_for_node(node_id)?;
    if rows.is_empty() && catalog.intake_by_id(intake_id)?.is_none() {
        return Err(OpsError::IntakeNotFound { intake_id });
    }
    Ok(rows.into_iter().map(AuditTrailEntry::from_row).collect())
}
