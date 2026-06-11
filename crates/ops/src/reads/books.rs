// SPDX-License-Identifier: Apache-2.0

//! Browse the book catalog: list / find / show / TOC / aggregate stats.
//!
//! Every function opens the catalog read-only per call and works whether
//! the [`Ops`] was built with a warm [`bookrack_query::Library`] or in
//! catalog-only mode. The implementation mirrors what
//! [`bookrack_query::Library`] does, but lives here so the CLI does not
//! have to pay the embedder probe before it can browse the catalog.

use std::collections::BTreeMap;

use bookrack_catalog::{Catalog, IntakeFilter, IntakeStatus};
use bookrack_core::{ItemKind, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;

use crate::Ops;
use crate::OpsError;
use crate::Result;
use crate::dto::{
    BookDetail, BookFilter, BookSummary, LibraryStats, ListBooksResult, MAX_TOC_NODES, Toc,
    TocNode, clamp_limit,
};
use crate::recorder::record_call_sync;

/// List books in catalog order, paginated.
pub fn list_books<E: Embedder>(ops: &Ops<E>, limit: u32, offset: u32) -> Result<ListBooksResult> {
    record_call_sync!(
        ops,
        "library.list_books",
        serde_json::json!({ "limit": limit, "offset": offset }),
        { find_books(ops, BookFilter::default(), limit, offset) }
    )
}

/// List books matching `filter`, paginated. The limit is clamped to
/// [`dto::MAX_LIST_LIMIT`](crate::dto::MAX_LIST_LIMIT); `truncated` is
/// set when the clamp took effect or when `total > offset + books.len()`.
pub fn find_books<E: Embedder>(
    ops: &Ops<E>,
    filter: BookFilter,
    limit: u32,
    offset: u32,
) -> Result<ListBooksResult> {
    let args = serde_json::json!({
        "title_substring": filter.title_substring,
        "contributor_name": filter.contributor_name,
        "contributor_role": filter.contributor_role,
        "format": filter.format,
        "statuses": filter
            .statuses
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>(),
        "limit": limit,
        "offset": offset,
    });
    record_call_sync!(ops, "library.find_books", args, {
        let (effective_limit, clamp_triggered) = clamp_limit(limit);
        let catalog = Catalog::open_read_only(ops.catalog_db())?;
        let catalog_filter = IntakeFilter {
            title_substring: filter.title_substring.as_deref(),
            contributor_name: filter.contributor_name.as_deref(),
            contributor_role: filter.contributor_role.as_deref(),
            statuses: filter.statuses.as_slice(),
            format: filter.format.as_deref(),
            ..IntakeFilter::default()
        };
        let intakes = catalog.find_intakes(&catalog_filter, effective_limit, offset)?;
        let total = catalog.count_find_intakes(&catalog_filter)?;
        let mut books = Vec::with_capacity(intakes.len());
        for intake in intakes {
            let effective =
                catalog.effective_publication_attrs(intake.intake_id, ItemKind::Book)?;
            let title = effective.get("title").map(str::to_string);
            let contributors =
                catalog.contributors_for_address(intake.intake_id, ItemKind::Book)?;
            let top_contributor = contributors.first().map(|c| c.name.clone());
            books.push(BookSummary::from_intake(&intake, title, top_contributor));
        }
        let returned = books.len() as u64;
        let truncated = clamp_triggered || u64::from(offset) + returned < total;
        Ok(ListBooksResult {
            books,
            total,
            truncated,
        })
    })
}

/// Fetch the full bibliographic record of one book by intake id.
///
/// Returns [`OpsError::IntakeNotFound`] when no such intake is registered.
pub fn show_book<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<BookDetail> {
    record_call_sync!(
        ops,
        "library.show_book",
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
            Ok(BookDetail::build(
                intake,
                effective,
                overrides,
                contributors,
            ))
        }
    )
}

/// Project the table of contents of one book — the organizing nodes
/// under the book root, in depth-first TOC order.
///
/// Returns [`OpsError::IntakeNotFound`] when no such intake is
/// registered. An intake that exists but has no organizing nodes
/// produces an empty [`Toc`] with `truncated = false`.
pub fn show_toc<E: Embedder>(ops: &Ops<E>, intake_id: i64) -> Result<Toc> {
    record_call_sync!(
        ops,
        "library.show_toc",
        serde_json::json!({ "intake_id": intake_id }),
        {
            let catalog = Catalog::open_read_only(ops.catalog_db())?;
            if catalog.intake_by_id(intake_id)?.is_none() {
                return Err(OpsError::IntakeNotFound { intake_id });
            }
            let corpus = Corpus::open(ops.corpus_db())?;
            let book_root_id = PartitionIdx::new(intake_id).root();
            let nodes = corpus.toc_for_book(book_root_id, MAX_TOC_NODES + 1)?;
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

/// Aggregate counts across the catalog.
pub fn show_stats<E: Embedder>(ops: &Ops<E>) -> Result<LibraryStats> {
    record_call_sync!(ops, "library.stats", serde_json::Value::Null, {
        let catalog = Catalog::open_read_only(ops.catalog_db())?;
        let mut intake_counts_by_status = BTreeMap::new();
        for status in IntakeStatus::ALL {
            let n = catalog.count_intakes_by_status(std::slice::from_ref(&status))?;
            intake_counts_by_status.insert(status.as_str().to_string(), n);
        }
        let mut intake_count_by_format = BTreeMap::new();
        for format in ["epub", "pdf", "mobi", "azw3", "txt"] {
            let n = catalog.count_intakes_by_format(format)?;
            if n > 0 {
                intake_count_by_format.insert(format.to_string(), n);
            }
        }
        let mut book_state_counts_by_stage = BTreeMap::new();
        for stage in [
            "extract",
            "structure",
            "metadata",
            "chunk",
            "embed",
            "ready",
        ] {
            let n = catalog.count_book_states_by_stage(stage)?;
            if n > 0 {
                book_state_counts_by_stage.insert(stage.to_string(), n);
            }
        }
        let mut retrieval_issue_counts_by_status = BTreeMap::new();
        for status in ["open", "triaged", "resolved", "wontfix"] {
            let n = catalog.count_retrieval_issues_by_status(&[status])?;
            if n > 0 {
                retrieval_issue_counts_by_status.insert(status.to_string(), n);
            }
        }
        let papers = papers_stats_if_configured(ops)?;
        Ok(LibraryStats {
            intake_counts_by_status,
            intake_count_by_format,
            book_state_counts_by_stage,
            retrieval_issue_counts_by_status,
            papers,
        })
    })
}

/// Optional paper-side aggregate section that piggybacks on
/// [`show_stats`]. Returns `None` when the calling `Ops` was built
/// without a papers backend; otherwise opens the paper catalog
/// read-only and counts intake rows per coarse lifecycle status.
fn papers_stats_if_configured<E: Embedder>(
    ops: &Ops<E>,
) -> Result<Option<bookrack_query::dto::PapersStats>> {
    let Some(papers_db) = ops.papers_catalog_db() else {
        return Ok(None);
    };
    let papers = Catalog::open_read_only(papers_db)?;
    let mut intake_counts_by_status = BTreeMap::new();
    for status in IntakeStatus::ALL {
        let n = papers.count_intakes_by_status(std::slice::from_ref(&status))?;
        intake_counts_by_status.insert(status.as_str().to_string(), n);
    }
    Ok(Some(bookrack_query::dto::PapersStats {
        intake_counts_by_status,
    }))
}
