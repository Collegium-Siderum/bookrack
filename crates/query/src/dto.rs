// SPDX-License-Identifier: Apache-2.0

//! DTOs the read-only query facade hands back to its consumers.
//!
//! The MCP server and the CLI both serialize these directly to JSON.
//! They are decoupled from the catalog row structs on purpose: a
//! catalog schema change (a renamed column, a new field) does not
//! break the MCP wire, because the projection happens here.
//!
//! Wire conventions:
//!
//! - List endpoints return [`ListBooksResult`] with the slice already
//!   clamped to [`MAX_LIST_LIMIT`]. The `truncated` flag is true when
//!   either the requested limit was clamped or `total > offset +
//!   books.len()`.
//! - `intake_id` (the catalog's surrogate key) is the universal book
//!   identifier on the wire. Vector partitions are derived from it via
//!   invariant I2; consumers never see the partition index.
//! - String fields are owned (`String`, not `&str`), so a DTO can be
//!   built once and returned through an `Arc`-shared facade.

use std::collections::BTreeMap;

use serde::Serialize;

use bookrack_catalog::{EffectiveAttrs, Intake, IntakeStatus, NodeContributor};
use bookrack_corpus::Node;

/// Server-side ceiling on a single list page. Larger requests are
/// silently clamped and the response carries `truncated = true` so the
/// caller can tell.
pub const MAX_LIST_LIMIT: u32 = 100;

/// Default page size when the caller does not specify one.
pub const DEFAULT_LIST_LIMIT: u32 = 20;

/// Maximum TOC nodes one [`Toc`] may carry. Books at the current
/// pilot scale fit well under this; the cap is a safety net against
/// pathological inputs and reflects in [`Toc::truncated`].
pub const MAX_TOC_NODES: usize = 2000;

/// One row of [`Catalog::list_books`] / [`Catalog::find_books`]: just
/// enough to render a list entry without a second fetch.
#[derive(Debug, Clone, Serialize)]
pub struct BookSummary {
    /// The catalog's surrogate key for this book; stable for life.
    pub intake_id: i64,
    /// Best-effort title for the book; `None` when neither
    /// `node_publication_attrs` nor a corpus root title is set.
    pub title: Option<String>,
    /// File format (`epub`, `pdf`, ...), if known.
    pub format: Option<String>,
    /// Coarse lifecycle status (`pending` / `extracted` / `embedded` /
    /// `dedup_hold` / `aborted`).
    pub status: String,
    /// First author (or other contributor) attributed at the book
    /// root, if any.
    pub top_contributor: Option<String>,
}

/// One [`Catalog::show_book`] response: the full bibliographic record
/// plus all contributors.
#[derive(Debug, Clone, Serialize)]
pub struct BookDetail {
    /// The catalog's surrogate key.
    pub intake_id: i64,
    /// Best-effort title for the book.
    pub title: Option<String>,
    /// File format, if known.
    pub format: Option<String>,
    /// Coarse lifecycle status.
    pub status: String,
    /// Effective bibliographic attributes — the base layer merged with
    /// any human override. Keys are stable strings (`title`,
    /// `publisher`, `isbn`, ...).
    pub effective_biblio: BTreeMap<String, String>,
    /// Every contributor attributed at the book root, in (role,
    /// ordinal) order.
    pub contributors: Vec<ContributorEntry>,
}

/// One contributor entry within a [`BookDetail`].
#[derive(Debug, Clone, Serialize)]
pub struct ContributorEntry {
    /// Role (`author`, `translator`, `editor`, ...).
    pub role: String,
    /// Position among contributors sharing this node and role.
    pub ordinal: i64,
    /// The contributor's name.
    pub name: String,
    /// Nationality, if recorded.
    pub nationality: Option<String>,
    /// Where this attribution came from (`extracted` /
    /// `extracted-filename` / `user`).
    pub origin: String,
}

/// One node entry within a [`Toc`], flattened from the corpus tree's
/// depth-first walk.
#[derive(Debug, Clone, Serialize)]
pub struct TocNode {
    /// Stable corpus node id of this TOC entry.
    pub node_id: i64,
    /// Parent's node id; `None` for the book root.
    pub parent_id: Option<i64>,
    /// Heading text.
    pub title: Option<String>,
    /// Tree depth; 0 is the book root.
    pub depth: i64,
    /// Position among siblings under the same parent.
    pub ordinal: i64,
    /// Low end of the document-order span this node covers.
    pub toc_lo: Option<i64>,
    /// High end of the document-order span this node covers.
    pub toc_hi: Option<i64>,
}

/// A book's table of contents as the facade serves it.
#[derive(Debug, Clone, Serialize)]
pub struct Toc {
    /// The book this TOC belongs to.
    pub intake_id: i64,
    /// Organizing nodes in depth-first TOC order. May be truncated to
    /// [`MAX_TOC_NODES`]; see [`Toc::truncated`].
    pub nodes: Vec<TocNode>,
    /// True when the underlying corpus had more organizing nodes than
    /// the cap.
    pub truncated: bool,
}

/// Aggregate counts behind the `library.stats` tool.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LibraryStats {
    /// Number of intake rows per coarse lifecycle status.
    pub intake_counts_by_status: BTreeMap<String, u64>,
    /// Number of intake rows per format (`epub`, `pdf`, ...). Rows
    /// whose `format` is `NULL` are excluded.
    pub intake_count_by_format: BTreeMap<String, u64>,
    /// Number of book-state rows per pipeline stage.
    pub book_state_counts_by_stage: BTreeMap<String, u64>,
    /// Number of retrieval-issue rows per triage status.
    pub retrieval_issue_counts_by_status: BTreeMap<String, u64>,
}

/// Result page for `library.list_books` / `library.find_books`.
#[derive(Debug, Clone, Serialize)]
pub struct ListBooksResult {
    /// Books in this page.
    pub books: Vec<BookSummary>,
    /// Total number of books matching the filter (regardless of
    /// pagination).
    pub total: u64,
    /// True when the page does not cover the full result set.
    pub truncated: bool,
}

/// Facade-level filter for `find_books`. Mirrors
/// [`bookrack_catalog::IntakeFilter`] but owns its strings so the
/// caller can build it once and pass it through a `Send` future.
#[derive(Debug, Default, Clone)]
pub struct BookFilter {
    /// Substring match against the book title.
    pub title_substring: Option<String>,
    /// Exact-equality match against a contributor name.
    pub contributor_name: Option<String>,
    /// Restrict the contributor JOIN to one role.
    pub contributor_role: Option<String>,
    /// Match against this set of lifecycle statuses.
    pub statuses: Vec<IntakeStatus>,
    /// Exact-equality match against the file format.
    pub format: Option<String>,
}

impl BookSummary {
    /// Project a catalog [`Intake`] row into a list summary, using the
    /// `title` and `top_contributor` resolved separately (the catalog
    /// row only carries identity / lifecycle fields).
    pub(crate) fn from_intake(
        intake: &Intake,
        title: Option<String>,
        top_contributor: Option<String>,
    ) -> BookSummary {
        BookSummary {
            intake_id: intake.intake_id,
            title,
            format: intake.format.clone(),
            status: intake.status.as_str().to_string(),
            top_contributor,
        }
    }
}

impl ContributorEntry {
    pub(crate) fn from_row(row: NodeContributor) -> ContributorEntry {
        ContributorEntry {
            role: row.role,
            ordinal: row.ordinal,
            name: row.name,
            nationality: row.nationality,
            origin: row.origin,
        }
    }
}

impl BookDetail {
    /// Project a catalog [`Intake`] plus its effective biblio and
    /// contributor rows into the detail DTO.
    pub(crate) fn build(
        intake: Intake,
        effective: EffectiveAttrs,
        contributors: Vec<NodeContributor>,
    ) -> BookDetail {
        let mut effective_biblio = BTreeMap::new();
        for (name, value) in effective.iter() {
            effective_biblio.insert(name.to_string(), value.to_string());
        }
        let title = effective_biblio.get("title").cloned();
        BookDetail {
            intake_id: intake.intake_id,
            title,
            format: intake.format,
            status: intake.status.as_str().to_string(),
            effective_biblio,
            contributors: contributors
                .into_iter()
                .map(ContributorEntry::from_row)
                .collect(),
        }
    }
}

impl TocNode {
    pub(crate) fn from_node(node: &Node) -> TocNode {
        TocNode {
            node_id: node.node_id.get(),
            parent_id: node.parent_id.map(|id| id.get()),
            title: node.title.clone(),
            depth: node.depth,
            ordinal: node.ordinal,
            toc_lo: node.toc_lo,
            toc_hi: node.toc_hi,
        }
    }
}

/// Clamp `requested` to [`MAX_LIST_LIMIT`] and return both the clamped
/// value and a flag set when clamping changed it. A `requested` of 0
/// becomes [`DEFAULT_LIST_LIMIT`] — a missing limit on the wire is
/// what `0` represents at this seam.
pub(crate) fn clamp_limit(requested: u32) -> (u32, bool) {
    let effective = if requested == 0 {
        DEFAULT_LIST_LIMIT
    } else {
        requested
    };
    if effective > MAX_LIST_LIMIT {
        (MAX_LIST_LIMIT, true)
    } else {
        (effective, false)
    }
}
