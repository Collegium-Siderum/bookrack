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

use bookrack_catalog::{EffectiveAttrs, Intake, IntakeStatus, NodeContributor, NodeOverride};
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

/// Maximum leaves on either side of the anchor a context-window read
/// may request. Larger requests are clamped and the response carries
/// `truncated = true`.
pub const MAX_CONTEXT_RADIUS: u32 = 20;

/// Character budget for the passage text one read response may carry.
/// A response stops adding passages once the budget is spent; the
/// caller pages with the returned cursor instead of receiving an
/// unbounded body.
pub const MAX_READ_CHARS: usize = 30_000;

/// Maximum leaf rows one span read fetches from the corpus. A backstop
/// behind [`MAX_READ_CHARS`]: the character budget normally fires
/// first, this cap bounds the row fetch when passages are tiny.
pub const MAX_SPAN_LEAVES: usize = 2000;

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
    /// Every active override at the book root, in field order. A field
    /// listed here owes its effective value (or, for `value: null`, its
    /// absence) to curation rather than extraction; fields not listed
    /// read straight from the extracted base layer.
    pub overrides: Vec<OverrideEntry>,
    /// Every contributor attributed at the book root, in (role,
    /// ordinal) order.
    pub contributors: Vec<ContributorEntry>,
}

/// One override entry within a [`BookDetail`].
#[derive(Debug, Clone, Serialize)]
pub struct OverrideEntry {
    /// The overridden field.
    pub field: String,
    /// The override value. `null` is a deliberate nullify: the
    /// extracted value is suppressed and the field has no effective
    /// value until a correct one is set.
    pub value: Option<String>,
    /// Whether the user has confirmed this override.
    pub confirmed: bool,
    /// When the override was last curated, ISO-8601 UTC.
    pub curated_at: String,
    /// Who curated the override (an `actor_kind` database string).
    pub curated_by: String,
    /// Free-form notes.
    pub notes: Option<String>,
}

/// One contributor entry within a [`BookDetail`].
#[derive(Debug, Clone, Serialize)]
pub struct ContributorEntry {
    /// Surrogate id of the contributor row; the address
    /// `contributor_remove` takes.
    pub contributor_id: i64,
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

/// One leaf of body text within a context-window or span read, in
/// document order.
#[derive(Debug, Clone, Serialize)]
pub struct Passage {
    /// Stable corpus node id of this leaf.
    pub node_id: i64,
    /// Leaf kind (`paragraph`, `heading`, `footnote`, `table`, ...).
    /// Structural leaves are returned alongside prose so the slice
    /// reproduces the book's reading order; callers filter by kind if
    /// they want prose only.
    pub node_type: String,
    /// Document-order position of this leaf within its book.
    pub toc_position: i64,
    /// Source page index the leaf starts on, if known.
    pub page_index_start: Option<i64>,
    /// The leaf's body text.
    pub text: String,
}

/// A window of leaves around one anchor leaf, in document order.
#[derive(Debug, Clone, Serialize)]
pub struct ContextWindow {
    /// The book the anchor belongs to.
    pub intake_id: i64,
    /// The leaf the window is centred on; always present in
    /// `passages`.
    pub anchor_node_id: i64,
    /// The window's leaves in document order, the anchor included.
    pub passages: Vec<Passage>,
    /// True when the requested radius was clamped or the character
    /// budget dropped leaves the window would otherwise carry.
    pub truncated: bool,
}

/// One page of an organizing node's body text, in document order.
#[derive(Debug, Clone, Serialize)]
pub struct SpanText {
    /// The book the node belongs to.
    pub intake_id: i64,
    /// The organizing node whose span is being read.
    pub node_id: i64,
    /// The organizing node's heading text.
    pub title: Option<String>,
    /// Low end of the node's document-order span; `None` when the
    /// node has no leaves.
    pub toc_lo: Option<i64>,
    /// High end of the node's document-order span; `None` when the
    /// node has no leaves.
    pub toc_hi: Option<i64>,
    /// This page's leaves in document order.
    pub passages: Vec<Passage>,
    /// Cursor for the next page: pass it back as `start_after` to
    /// resume. `None` when this page completes the span.
    pub next_offset: Option<i64>,
    /// True when more of the span remains past this page —
    /// equivalent to `next_offset.is_some()`.
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
    /// Aggregate counts for the paper-side stack, set when the
    /// calling `Ops` has a papers backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub papers: Option<PapersStats>,
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
    pub fn from_intake(
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
    /// Project a [`NodeContributor`] row into a wire-ready entry.
    pub fn from_row(row: NodeContributor) -> ContributorEntry {
        ContributorEntry {
            contributor_id: row.contributor_id,
            role: row.role,
            ordinal: row.ordinal,
            name: row.name,
            nationality: row.nationality,
            origin: row.origin,
        }
    }
}

impl OverrideEntry {
    /// Project a [`NodeOverride`] row into a wire-ready entry.
    pub fn from_row(row: NodeOverride) -> OverrideEntry {
        OverrideEntry {
            field: row.field,
            value: row.value,
            confirmed: row.confirmed,
            curated_at: row.curated_at,
            curated_by: row.curated_by,
            notes: row.notes,
        }
    }
}

impl BookDetail {
    /// Project a catalog [`Intake`] plus its effective biblio,
    /// override, and contributor rows into the detail DTO.
    pub fn build(
        intake: Intake,
        effective: EffectiveAttrs,
        overrides: Vec<NodeOverride>,
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
            overrides: overrides.into_iter().map(OverrideEntry::from_row).collect(),
            contributors: contributors
                .into_iter()
                .map(ContributorEntry::from_row)
                .collect(),
        }
    }
}

impl Passage {
    /// Project a corpus leaf [`Node`] into a wire-ready passage. The
    /// caller guarantees the node carries a document-order position
    /// (the range query only returns such rows).
    pub fn from_node(node: &Node) -> Passage {
        Passage {
            node_id: node.node_id.get(),
            node_type: node.node_type.as_str().to_string(),
            toc_position: node.toc_lo.unwrap_or_default(),
            page_index_start: node.page_index_start,
            text: node.text_content.clone().unwrap_or_default(),
        }
    }
}

impl TocNode {
    /// Project a corpus [`Node`] into a TOC entry.
    pub fn from_node(node: &Node) -> TocNode {
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
pub fn clamp_limit(requested: u32) -> (u32, bool) {
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

/// One row of `library.list_papers` / `library.find_papers`. Mirrors
/// [`BookSummary`] for the paper pipeline, with the four fields a
/// Triage caller needs to disambiguate at a glance: DOI, arXiv id,
/// container title, and year.
#[derive(Debug, Clone, Serialize)]
pub struct PaperSummary {
    /// The catalog's surrogate key for this paper.
    pub intake_id: i64,
    /// Best-effort title for the paper.
    pub title: Option<String>,
    /// File format (`pdf`, ...).
    pub format: Option<String>,
    /// Coarse lifecycle status.
    pub status: String,
    /// First author attributed at the paper root, if any.
    pub top_contributor: Option<String>,
    /// DOI from the publication-attrs row, if known.
    pub doi: Option<String>,
    /// arXiv identifier in canonical form (no `arXiv:` prefix), if known.
    pub arxiv_id: Option<String>,
    /// Container title (journal, conference proceedings, ...), if known.
    pub container_title: Option<String>,
    /// Year string as carried by the publication-attrs row.
    pub year: Option<String>,
}

/// One `library.show_paper` response. Mirrors [`BookDetail`] for the
/// paper pipeline and carries the abstract text alongside the
/// effective biblio and contributor rows. The abstract is in the
/// detail response only — list responses stay slim.
#[derive(Debug, Clone, Serialize)]
pub struct PaperDetail {
    /// The catalog's surrogate key.
    pub intake_id: i64,
    /// Best-effort title for the paper.
    pub title: Option<String>,
    /// File format, if known.
    pub format: Option<String>,
    /// Coarse lifecycle status.
    pub status: String,
    /// Effective bibliographic attributes (paper scope), merged with
    /// any human override.
    pub effective_biblio: BTreeMap<String, String>,
    /// Every active override at the paper root.
    pub overrides: Vec<OverrideEntry>,
    /// Every contributor attributed at the paper root, in (role,
    /// ordinal) order.
    pub contributors: Vec<ContributorEntry>,
    /// Abstract body, when the IDENTIFY pass found one.
    pub abstract_text: Option<String>,
}

/// One `papers.fetch_source` response. Carries the locator of one
/// paper's archived source PDF — its absolute on-disk path, its
/// size, and the SHA-256 captured at REGISTER — so the calling
/// client can open the bytes directly via `fs::read`. The bytes
/// themselves never flow through the control plane.
#[derive(Debug, Clone, Serialize)]
pub struct PaperSource {
    /// Catalog intake id of the paper this source belongs to.
    pub intake_id: i64,
    /// Absolute path to the archived PDF on disk.
    pub path: String,
    /// File size in bytes, read from the archived file's metadata at
    /// call time.
    pub bytes_size: i64,
    /// SHA-256 of the source PDF, copied verbatim from
    /// `intake.source_sha256`. It is the hash captured at REGISTER,
    /// not a re-hash of the on-disk copy; clients that want to verify
    /// the archive's integrity hash the bytes they read.
    pub sha256: String,
}

/// Facade-level filter for `find_papers`. Mirrors
/// [`bookrack_catalog::IntakeFilter`] for the paper-side scope and
/// owns its strings so the caller can build it once and pass it
/// through a `Send` future.
#[derive(Debug, Default, Clone)]
pub struct PaperFilter {
    /// Substring match against the paper title.
    pub title_substring: Option<String>,
    /// Exact-equality match against a contributor name.
    pub contributor_name: Option<String>,
    /// Exact-equality match against the year column.
    pub year: Option<String>,
    /// Substring match against the container title.
    pub venue_substring: Option<String>,
    /// Exact-equality match against the DOI.
    pub doi: Option<String>,
}

/// Result page for `library.list_papers` / `library.find_papers`.
#[derive(Debug, Clone, Serialize)]
pub struct ListPapersResult {
    /// Papers in this page.
    pub papers: Vec<PaperSummary>,
    /// Total number of papers matching the filter.
    pub total: u64,
    /// True when the page does not cover the full result set.
    pub truncated: bool,
}

/// Paper-side aggregate counts attached to [`LibraryStats`] as an
/// optional companion section. Set when the calling `Ops` has a
/// papers backend; absent otherwise.
#[derive(Debug, Clone, Serialize)]
pub struct PapersStats {
    /// Number of paper intake rows per coarse lifecycle status.
    pub intake_counts_by_status: BTreeMap<String, u64>,
}

impl PaperSummary {
    /// Project a catalog [`Intake`] row plus a few paper-specific
    /// fields read from the effective publication-attrs view into a
    /// list summary.
    pub fn from_intake(
        intake: &Intake,
        title: Option<String>,
        top_contributor: Option<String>,
        doi: Option<String>,
        arxiv_id: Option<String>,
        container_title: Option<String>,
        year: Option<String>,
    ) -> PaperSummary {
        PaperSummary {
            intake_id: intake.intake_id,
            title,
            format: intake.format.clone(),
            status: intake.status.as_str().to_string(),
            top_contributor,
            doi,
            arxiv_id,
            container_title,
            year,
        }
    }
}

impl PaperDetail {
    /// Project a catalog [`Intake`] plus its effective biblio,
    /// override, and contributor rows into the paper detail DTO.
    pub fn build(
        intake: Intake,
        effective: EffectiveAttrs,
        overrides: Vec<NodeOverride>,
        contributors: Vec<NodeContributor>,
    ) -> PaperDetail {
        let mut effective_biblio = BTreeMap::new();
        for (name, value) in effective.iter() {
            effective_biblio.insert(name.to_string(), value.to_string());
        }
        let title = effective_biblio.get("title").cloned();
        let abstract_text = effective_biblio.get("abstract_text").cloned();
        PaperDetail {
            intake_id: intake.intake_id,
            title,
            format: intake.format,
            status: intake.status.as_str().to_string(),
            effective_biblio,
            overrides: overrides.into_iter().map(OverrideEntry::from_row).collect(),
            contributors: contributors
                .into_iter()
                .map(ContributorEntry::from_row)
                .collect(),
            abstract_text,
        }
    }
}
