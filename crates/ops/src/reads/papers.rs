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
    ListPapersResult, MAX_TOC_NODES, PaperDetail, PaperFilter, PaperSummary, Toc, TocNode,
    clamp_limit,
};
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
/// [`crate::dto::MAX_LIST_LIMIT`]; `truncated` is set when the clamp
/// took effect or when `total > offset + papers.len()`.
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
        let (effective_limit, clamp_triggered) = clamp_limit(limit);
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
        let intakes = catalog.find_intakes(&catalog_filter, effective_limit, offset)?;
        let total = catalog.count_find_intakes(&catalog_filter)?;
        let mut papers = Vec::with_capacity(intakes.len());
        for intake in intakes {
            let effective =
                catalog.effective_publication_attrs(intake.intake_id, ItemKind::Paper)?;
            let title = effective.get("title").map(str::to_string);
            let doi = effective.get("doi").map(str::to_string);
            let arxiv_id = effective.get("arxiv_id").map(str::to_string);
            let container_title = effective.get("container_title").map(str::to_string);
            let year = effective.get("year").map(str::to_string);
            let contributors =
                catalog.contributors_for_address(intake.intake_id, ItemKind::Paper)?;
            let top_contributor = contributors.first().map(|c| c.name.clone());
            papers.push(PaperSummary::from_intake(
                &intake,
                title,
                top_contributor,
                doi,
                arxiv_id,
                container_title,
                year,
            ));
        }
        let returned = papers.len() as u64;
        let truncated = clamp_triggered || u64::from(offset) + returned < total;
        Ok(ListPapersResult {
            papers,
            total,
            truncated,
        })
    })
}

/// Fetch the full bibliographic record of one paper by intake id.
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
            let catalog = Catalog::open_read_only(papers_db)?;
            let Some(intake) = catalog.intake_by_id(intake_id)? else {
                return Err(OpsError::IntakeNotFound { intake_id });
            };
            let effective =
                catalog.effective_publication_attrs(intake.intake_id, ItemKind::Paper)?;
            let overrides = catalog.overrides_for_address(intake.intake_id, ItemKind::Paper)?;
            let contributors =
                catalog.contributors_for_address(intake.intake_id, ItemKind::Paper)?;
            Ok(PaperDetail::build(
                intake,
                effective,
                overrides,
                contributors,
            ))
        }
    )
}

/// Project the table of contents of one paper. Papers carry one Work
/// root plus one prose leaf, so the TOC is effectively empty for a
/// well-formed paper. The shape is shared with the book TOC for
/// uniformity at the wire boundary.
///
/// Returns [`OpsError::IntakeNotFound`] when no such intake is
/// registered on the paper catalog, or
/// [`OpsError::PapersBackendNotConfigured`] when the calling `Ops`
/// has no papers backend.
pub fn show_paper_toc<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<Toc> {
    record_call_sync!(
        ops,
        "library.show_paper_toc",
        serde_json::json!({ "intake_id": intake_id }),
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
            let nodes = corpus.toc_for_book(work_root_id, MAX_TOC_NODES + 1)?;
            let truncated = nodes.len() > MAX_TOC_NODES;
            let projected: Vec<TocNode> = nodes
                .iter()
                .take(MAX_TOC_NODES)
                .map(TocNode::from_node)
                .collect();
            Ok(Toc {
                intake_id,
                nodes: projected,
                truncated,
            })
        }
    )
}
