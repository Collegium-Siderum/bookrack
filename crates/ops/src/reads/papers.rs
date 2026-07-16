// SPDX-License-Identifier: Apache-2.0

//! Browse the paper catalog: list / find / show / TOC.
//!
//! Mirrors [`super::books`] but works against the paper-side stack
//! attached to an [`Ops`] via [`crate::Ops::with_papers`]. Each
//! function opens the paper catalog read-only per call; without a
//! configured papers backend every function returns
//! [`OpsError::PapersBackendNotConfigured`] before opening anything.

use bookrack_catalog::{Catalog, IntakeFilter};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::{
    ListPapersResult, PaperAuditInfo, PaperDetail, PaperFilter, PaperSource, PaperSummary,
    ShowTocArgs, Toc, TocNodes, TocStats, clamp_limit,
};
use crate::reads::books::toc_call_args;
use crate::recorder::record_call_sync;

/// List papers in catalog order, paginated.
pub fn list_papers<E: Embedder>(ops: &Ops<E>, limit: u32, offset: u32) -> Result<ListPapersResult> {
    record_call_sync!(
        ops,
        "library.list_papers",
        serde_json::json!({ "limit": limit, "offset": offset }),
        { find_papers(ops, PaperFilter::default(), limit, offset) }
    )
}

/// List papers matching `filter`, paginated. The limit is clamped to
/// [`crate::dto::MAX_LIST_LIMIT`]; `truncated` is set when the page
/// does not cover the full filter result.
pub fn find_papers<E: Embedder>(
    ops: &Ops<E>,
    filter: PaperFilter,
    limit: u32,
    offset: u32,
) -> Result<ListPapersResult> {
    let args = serde_json::json!({
        "title_substring": filter.title_substring,
        "contributor_name": filter.contributor_name,
        "year": filter.year,
        "venue_substring": filter.venue_substring,
        "doi": filter.doi,
        "limit": limit,
        "offset": offset,
    });
    record_call_sync!(ops, "library.find_papers", args, {
        let papers_db = ops
            .papers_catalog_db()
            .ok_or(OpsError::PapersBackendNotConfigured)?;
        let (effective_limit, _) = clamp_limit(limit);
        let catalog = Catalog::open_read_only(papers_db)?;
        let catalog_filter = IntakeFilter {
            kind: ItemKind::Paper,
            title_substring: filter.title_substring.as_deref(),
            contributor_name: filter.contributor_name.as_deref(),
            year: filter.year.as_deref(),
            venue_substring: filter.venue_substring.as_deref(),
            doi: filter.doi.as_deref(),
            ..IntakeFilter::default()
        };
        let (intakes, total) =
            catalog.find_intakes_page(&catalog_filter, effective_limit, offset)?;
        let intake_ids: Vec<i64> = intakes.iter().map(|i| i.intake_id).collect();
        let effective =
            catalog.effective_publication_attrs_for_intakes(&intake_ids, ItemKind::Paper)?;
        let contributors = catalog.contributors_for_addresses(&intake_ids, ItemKind::Paper)?;
        let papers: Vec<PaperSummary> = intakes
            .iter()
            .map(|intake| {
                let attrs = effective.get(&intake.intake_id);
                let title = attrs.and_then(|e| e.get("title").map(str::to_string));
                let doi = attrs.and_then(|e| e.get("doi").map(str::to_string));
                let arxiv_id = attrs.and_then(|e| e.get("arxiv_id").map(str::to_string));
                let container_title =
                    attrs.and_then(|e| e.get("container_title").map(str::to_string));
                let year = attrs.and_then(|e| e.get("year").map(str::to_string));
                let top_contributor = contributors
                    .get(&intake.intake_id)
                    .and_then(|cs| cs.first())
                    .map(|c| c.name.clone());
                PaperSummary::from_intake(
                    intake,
                    title,
                    top_contributor,
                    doi,
                    arxiv_id,
                    container_title,
                    year,
                )
            })
            .collect();
        let returned = papers.len() as u64;
        let truncated = u64::from(offset) + returned < total;
        Ok(ListPapersResult {
            papers,
            total,
            truncated,
        })
    })
}

/// Fetch the full bibliographic record of one paper by intake id,
/// including the aggregate shape of its ingested TOC (`None` when the
/// paper has no corpus nodes).
///
/// Returns [`OpsError::IntakeNotFound`] when no such intake is
/// registered on the paper catalog, or
/// [`OpsError::PapersBackendNotConfigured`] when the calling `Ops`
/// has no papers backend.
pub fn show_paper<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<PaperDetail> {
    record_call_sync!(
        ops,
        "library.show_paper",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let papers_db = ops
                .papers_catalog_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let corpus_db = ops
                .papers_corpus_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let catalog = Catalog::open_read_only(papers_db)?;
            let Some(intake) = catalog.intake_by_id(intake_id)? else {
                return Err(OpsError::IntakeNotFound { intake_id });
            };
            let effective =
                catalog.effective_publication_attrs(intake.intake_id, ItemKind::Paper)?;
            let overrides = catalog.overrides_for_address(intake.intake_id, ItemKind::Paper)?;
            let contributors =
                catalog.contributors_for_address(intake.intake_id, ItemKind::Paper)?;
            let audit = catalog
                .node_paper_audit(intake.intake_id, ItemKind::Paper.as_scope_str())?
                .map(|row| PaperAuditInfo {
                    verdict: row.verdict,
                    confidence: row.confidence,
                    audited_at: row.audited_at,
                    profile_name: row.profile_name,
                    profile_fingerprint: row.profile_fingerprint,
                });
            let corpus = Corpus::open(corpus_db)?;
            let toc_stats = corpus
                .toc_stats_for_book(PartitionIdx::new(intake_id).root())?
                .map(TocStats::from);
            Ok(PaperDetail::build(
                intake,
                effective,
                overrides,
                contributors,
                audit,
                toc_stats,
            ))
        }
    )
}

/// Project the table of contents of one paper, paginated. Papers
/// carry one Work root plus one prose leaf, so the TOC is effectively
/// empty for a well-formed paper. The shape and the pagination
/// contract are shared with the book TOC for uniformity at the wire
/// boundary; see [`crate::reads::books::show_toc`].
///
/// Returns [`OpsError::IntakeNotFound`] when no such intake is
/// registered on the paper catalog, or
/// [`OpsError::PapersBackendNotConfigured`] when the calling `Ops`
/// has no papers backend.
pub fn show_paper_toc<E: Embedder>(
    ops: &Ops<E>,
    intake_id: i64,
    args: &ShowTocArgs,
) -> Result<Toc> {
    record_call_sync!(
        ops,
        "library.show_paper_toc",
        toc_call_args(intake_id, args),
        {
            let papers_db = ops
                .papers_catalog_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let corpus_db = ops
                .papers_corpus_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let catalog = Catalog::open_read_only(papers_db)?;
            if catalog.intake_by_id(intake_id)?.is_none() {
                return Err(OpsError::IntakeNotFound { intake_id });
            }
            let corpus = Corpus::open(corpus_db)?;
            let work_root_id = PartitionIdx::new(intake_id).root();
            let q = args.to_query();
            let total = corpus.count_toc_nodes(work_root_id, &q)?;
            let nodes = corpus.toc_for_book(work_root_id, &q)?;
            let projected = TocNodes::project(&nodes, args.titles_only);
            let end = u64::from(args.offset) + projected.len() as u64;
            let next_offset = if end < total {
                u32::try_from(end).ok()
            } else {
                None
            };
            Ok(Toc {
                intake_id,
                nodes: projected,
                total,
                truncated: next_offset.is_some(),
                next_offset,
            })
        }
    )
}

/// Project one paper's base bibliographic row + contributors onto a
/// CSL-JSON item. The export is committed-row only: human overrides
/// are not merged in, because the CSL adapter operates on the
/// catalog's `PublicationAttrs` struct.
///
/// Returns [`OpsError::IntakeNotFound`] when no such intake is
/// registered on the paper catalog, or
/// [`OpsError::PapersBackendNotConfigured`] when the calling `Ops`
/// has no papers backend.
pub fn export_csl<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<bookrack_catalog::CslItem> {
    record_call_sync!(
        ops,
        "papers.export_csl",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let papers_db = ops
                .papers_catalog_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let catalog = Catalog::open_read_only(papers_db)?;
            if catalog.intake_by_id(intake_id)?.is_none() {
                return Err(OpsError::IntakeNotFound { intake_id });
            }
            let Some(attrs) = catalog.publication_attrs(intake_id, ItemKind::Paper)? else {
                return Err(OpsError::IntakeNotFound { intake_id });
            };
            let contributors = catalog.contributors_for_address(intake_id, ItemKind::Paper)?;
            Ok(bookrack_catalog::csl_from_catalog(&attrs, &contributors))
        }
    )
}

/// Resolve the locator for one paper's archived source PDF: its
/// absolute on-disk path, byte size, and the SHA-256 captured at
/// REGISTER. The bytes themselves do not flow through this call —
/// the caller opens the returned path with `fs::read`.
///
/// Returns [`OpsError::IntakeNotFound`] when no such intake is
/// registered on the paper catalog, [`OpsError::SourceNotArchived`]
/// when the intake exists but its `source_pdf_path` column is NULL
/// (the glean run was configured with `keep_source_pdf = false`, or
/// the row predates Phase 0), and
/// [`OpsError::PapersBackendNotConfigured`] when the calling `Ops`
/// has no papers backend.
pub fn fetch_source<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<PaperSource> {
    record_call_sync!(
        ops,
        "papers.fetch_source",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let papers_db = ops
                .papers_catalog_db()
                .ok_or(OpsError::PapersBackendNotConfigured)?;
            let catalog = Catalog::open_read_only(papers_db)?;
            let Some(intake) = catalog.intake_by_id(intake_id)? else {
                return Err(OpsError::IntakeNotFound { intake_id });
            };
            let path = intake
                .source_pdf_path
                .ok_or(OpsError::SourceNotArchived { intake_id })?;
            let bytes_size = std::fs::metadata(&path)
                .map(|m| m.len() as i64)
                .map_err(|e| OpsError::Other(e.into()))?;
            Ok(PaperSource {
                intake_id,
                path,
                bytes_size,
                sha256: intake.source_sha256,
            })
        }
    )
}
