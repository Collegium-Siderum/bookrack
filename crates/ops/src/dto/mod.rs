// SPDX-License-Identifier: Apache-2.0

//! DTOs for [`Ops`](crate::Ops).
//!
//! Read-side shapes are re-exported from `bookrack-query::dto` so a single
//! contract serves CLI `--json` output and MCP tool responses. Write-side
//! shapes and audit-trail / pipeline-trail entries live in this crate.

pub mod audit;
pub mod info;
pub mod metadata_report;
pub mod vectors_status;
pub mod writes;

pub use bookrack_query::dto::{
    BookDetail, BookFilter, BookSummary, ContextWindow, ContributorEntry, DEFAULT_LIST_LIMIT,
    LibraryStats, ListBooksResult, ListPapersResult, MAX_CONTEXT_RADIUS, MAX_LIST_LIMIT,
    MAX_READ_CHARS, MAX_SPAN_LEAVES, MAX_TOC_NODES, PaperDetail, PaperFilter, PaperSource,
    PaperSummary, PapersStats, Passage, SpanText, Toc, TocNode, clamp_limit,
};
