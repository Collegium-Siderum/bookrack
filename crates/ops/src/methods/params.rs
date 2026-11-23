// SPDX-License-Identifier: Apache-2.0

//! Parameter types of the library methods, declared once for both
//! transports.
//!
//! Each type derives `Deserialize` for the dispatcher and `JsonSchema`
//! for the method table; a field's doc comment is the parameter's
//! description in the published schema, which is all a client reads
//! before deciding whether a parameter is worth sending. Every type
//! that carries a library selection implements [`RoutedParams`], and
//! the table reads the selection key from that impl.
//!
//! The book-side metadata writes are the one place the two transports
//! still take different shapes: the control plane addresses the book as
//! `book` and leaves `reason` and `library` optional, MCP addresses it
//! as `intake_id` and requires both. Each write therefore has two types
//! here, the control plane's named after `metadata.*` and MCP's after
//! `library.metadata.*`.

// Parallel implementation: metadata-write-param-mirror.

use bookrack_core::{ItemKind, Problem};
use serde::Deserialize;

use super::RoutedParams;
use crate::SearchOptions;
use crate::dto::ShowTocArgs;
use crate::reads::reference::ReferenceLookup;
use crate::reads::search::{ExclusionKindMismatch, exclusions_to_partitions, validate_exclusions};
use crate::writes::reference::ReferenceOverlaySetRequest;

/// A refusal the dispatcher makes on a call's params before any
/// operation runs.
#[derive(Debug, thiserror::Error)]
pub enum ParamsError {
    /// The params do not deserialize into the method's parameter type.
    /// `method` is the wire name the call used.
    #[error("invalid {method} params: {message}")]
    Malformed {
        /// The method or tool name the call was addressed to.
        method: &'static str,
        /// What the deserializer refused, as it words it.
        message: String,
    },
    /// A search asked for a side of the library that does not exist.
    #[error("{method}: kind={kind:?} is not one of \"book\", \"paper\", \"all\"")]
    UnknownSearchKind {
        /// The method or tool name the call was addressed to.
        method: &'static str,
        /// The kind the call named.
        kind: String,
    },
    /// A search named exclusions of the side it does not search.
    #[error(transparent)]
    Exclusion(#[from] ExclusionKindMismatch),
}

impl bookrack_core::Explain for ParamsError {
    fn explain(&self) -> Problem {
        Problem::from_error_chain(self)
    }
}

/// Implement [`RoutedParams`] for params types whose selection is an
/// optional `library`.
macro_rules! routed_by_optional_library {
    ($( $t:ty ),+ $(,)?) => {
        $(
            impl RoutedParams for $t {
                const LIBRARY_KEY: &'static str = "library";

                fn library(&self) -> Option<&str> {
                    self.library.as_deref()
                }
            }
        )+
    };
}

/// Implement [`RoutedParams`] for params types that require `library`.
macro_rules! routed_by_required_library {
    ($( $t:ty ),+ $(,)?) => {
        $(
            impl RoutedParams for $t {
                const LIBRARY_KEY: &'static str = "library";

                fn library(&self) -> Option<&str> {
                    Some(self.library.as_str())
                }
            }
        )+
    };
}

/// Arguments for `library.stats` and `library.categories`. Carries only
/// the library selector; the tools themselves accept no other inputs.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct LibraryOnlyParams {
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `library.info`, which selects its
/// library under `name` rather than `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct LibraryInfoParams {
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub name: Option<String>,
}

impl RoutedParams for LibraryInfoParams {
    const LIBRARY_KEY: &'static str = "name";

    fn library(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Arguments for the `library.search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// The natural-language query to search the library for.
    pub query: String,
    /// Maximum number of passages to return. Defaults to the server's
    /// configured top-k when omitted.
    pub top_k: Option<usize>,
    /// Force a brute-force scan, ignoring any ANN index. Useful for
    /// ground-truth checks.
    #[serde(default)]
    pub bypass_index: bool,
    /// Override the IVF probe count for this query only. Higher values
    /// trade latency for recall; the persisted meta default applies when
    /// omitted.
    #[serde(default)]
    pub nprobes: Option<usize>,
    /// Override the IVF-PQ refinement multiplier for this query only.
    /// The persisted meta default applies when omitted.
    #[serde(default)]
    pub refine_factor: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
    /// Which side of the library to search: `"book"` (the default —
    /// existing behaviour), `"paper"` (only the paper-side store),
    /// or `"all"` (both stores, merged by ascending distance).
    #[serde(default)]
    pub kind: Option<String>,
    /// Catalog intake ids of books whose passages must not be returned.
    /// Omit or pass an empty list to filter nothing. Applies to `kind`
    /// `"book"` and `"all"`; passing it with `kind="paper"` is an
    /// error, since the two id name spaces are unrelated. Ids that name
    /// no book simply exclude nothing. `library.search_in_book` takes
    /// no exclusions — recall is already confined to one book there.
    #[serde(default)]
    pub exclude_book_intake_ids: Vec<i64>,
    /// Catalog intake ids of papers whose passages must not be
    /// returned. The paper-side mirror of `exclude_book_intake_ids`:
    /// applies to `kind` `"paper"` and `"all"`, and passing it with
    /// `kind="book"` is an error.
    #[serde(default)]
    pub exclude_paper_intake_ids: Vec<i64>,
}

impl SearchParams {
    /// The side of the library to search, with the exclusion lists
    /// checked against it: naming the other side's ids is refused
    /// rather than silently ignored. An unrecognized kind passes through
    /// here and is refused where the side is chosen.
    pub fn checked_kind(&self) -> Result<&str, ParamsError> {
        let kind = self.kind.as_deref().unwrap_or("book");
        validate_exclusions(
            kind,
            &self.exclude_book_intake_ids,
            &self.exclude_paper_intake_ids,
        )?;
        Ok(kind)
    }

    /// The override fields as [`SearchOptions`], excluding the items
    /// `exclude` names — the ids of whichever side the search runs on.
    pub fn overrides_excluding(&self, exclude: &[i64]) -> SearchOptions {
        SearchOptions {
            bypass_index: self.bypass_index,
            nprobes: self.nprobes,
            refine_factor: self.refine_factor,
            exclude_partitions: exclusions_to_partitions(exclude),
        }
    }
}

/// Arguments for the `library.search_in_book` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchInBookParams {
    /// Catalog intake id of the book to confine recall to.
    pub intake_id: i64,
    /// The natural-language query.
    pub query: String,
    /// Maximum number of passages to return.
    pub top_k: Option<usize>,
    /// Force a brute-force scan, ignoring any ANN index.
    #[serde(default)]
    pub bypass_index: bool,
    /// Override the IVF probe count for this query only.
    #[serde(default)]
    pub nprobes: Option<usize>,
    /// Override the IVF-PQ refinement multiplier for this query only.
    #[serde(default)]
    pub refine_factor: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.search_in_paper` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchInPaperParams {
    /// Catalog intake id of the paper to confine recall to.
    pub intake_id: i64,
    /// The natural-language query.
    pub query: String,
    /// Maximum number of passages to return.
    pub top_k: Option<usize>,
    /// Force a brute-force scan, ignoring any ANN index.
    #[serde(default)]
    pub bypass_index: bool,
    /// Override the IVF probe count for this query only.
    #[serde(default)]
    pub nprobes: Option<usize>,
    /// Override the IVF-PQ refinement multiplier for this query only.
    #[serde(default)]
    pub refine_factor: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// The override fields of a search confined to one item. Such a search
/// has nothing to exclude, so the confined searches carry no exclusion
/// fields.
fn confined_overrides(
    bypass_index: bool,
    nprobes: Option<usize>,
    refine_factor: Option<u32>,
) -> SearchOptions {
    SearchOptions {
        bypass_index,
        nprobes,
        refine_factor,
        exclude_partitions: Vec::new(),
    }
}

impl SearchInBookParams {
    /// The override fields as [`SearchOptions`].
    pub fn overrides(&self) -> SearchOptions {
        confined_overrides(self.bypass_index, self.nprobes, self.refine_factor)
    }
}

impl SearchInPaperParams {
    /// The override fields as [`SearchOptions`].
    pub fn overrides(&self) -> SearchOptions {
        confined_overrides(self.bypass_index, self.nprobes, self.refine_factor)
    }
}

/// Arguments for the `library.list_books` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListBooksParams {
    /// Maximum number of books in this page. Server-side cap applies.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.list_papers` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListPapersParams {
    /// Maximum number of papers in this page. Server-side cap applies.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for `library.list_pending_reviews` and
/// `library.list_paper_pending_reviews`: one page of the review queue.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PageParams {
    /// Maximum number of rows in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.find_books` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindBooksParams {
    /// Substring match against the book title as reported, i.e. with
    /// the curator's corrections applied.
    #[serde(default)]
    pub title_substring: Option<String>,
    /// Exact-equality match against a contributor name.
    #[serde(default)]
    pub contributor_name: Option<String>,
    /// Restrict the contributor filter to one role (`author` /
    /// `translator` / `editor` / `other`). Only takes effect with
    /// `contributor_name`.
    #[serde(default)]
    pub contributor_role: Option<String>,
    /// Exact-equality match against the file format (`epub`, `pdf`,
    /// `rule-text`, ...).
    #[serde(default)]
    pub format: Option<String>,
    /// Match books whose lifecycle status is one of these, named as
    /// they are reported in a row's `status`: `pending`, `extracted`,
    /// `dedup_hold`, `embedded`, `aborted`, `needs_ocr`. Only an
    /// `embedded` book has vectors, so only an `embedded` book can be
    /// recalled by the search tools. An unrecognised name is refused
    /// rather than dropped.
    #[serde(default)]
    pub statuses: Option<Vec<String>>,
    /// Match books carrying at least one of these category tags. An
    /// empty list, like an absent one, imposes no category filter. The
    /// tags a library carries are listed by `library.categories`; a tag
    /// not listed there matches nothing.
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// Match books whose reported language is one of these, compared
    /// as text against the value the row carries (`de`, `en`, ...).
    /// The pipeline does not normalise the tag, so match what
    /// `show_book` reports.
    #[serde(default)]
    pub language: Option<Vec<String>>,
    /// When true, match only scan sources the library holds no text
    /// for: a `needs_ocr` book with no book derived from it that was
    /// ingested. Each row then carries an `awaiting_text` object with
    /// the path of the scan, its SHA-256, its page count, and why its
    /// text layer was rejected. `statuses: ["needs_ocr"]` alone also
    /// lists the scans whose text is already in the library.
    #[serde(default)]
    pub awaiting_text: Option<bool>,
    /// Maximum number of books in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.find_papers` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindPapersParams {
    /// Substring match against the paper title as reported, i.e. with
    /// the curator's corrections applied. The three bibliographic
    /// filters below read the same layer.
    #[serde(default)]
    pub title_substring: Option<String>,
    /// Exact-equality match against a contributor name.
    #[serde(default)]
    pub contributor_name: Option<String>,
    /// Restrict the contributor filter to one role (`author` /
    /// `translator` / `editor` / `other`). Only takes effect with
    /// `contributor_name`.
    #[serde(default)]
    pub contributor_role: Option<String>,
    /// Match papers whose lifecycle status is one of these, named as
    /// they are reported in a row's `status`: `pending`, `extracted`,
    /// `embedded`. Only an `embedded` paper has vectors, so only an
    /// `embedded` paper can be recalled by the search tools. The book
    /// pipeline's three further states are not reachable here and are
    /// refused, as is an unrecognised name.
    #[serde(default)]
    pub statuses: Option<Vec<String>>,
    /// Match papers whose reported language is one of these, compared
    /// as text against the value the row carries (`de`, `en`, ...).
    /// The pipeline does not normalise the tag, so match what
    /// `show_paper` reports.
    #[serde(default)]
    pub language: Option<Vec<String>>,
    /// Exact-equality match against the reported year.
    #[serde(default)]
    pub year: Option<String>,
    /// Substring match against the reported container title (journal,
    /// proceedings, ...).
    #[serde(default)]
    pub venue_substring: Option<String>,
    /// Exact-equality match against the reported DOI.
    #[serde(default)]
    pub doi: Option<String>,
    /// Maximum number of papers in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the tools that address one book by intake id.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BookIdParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the tools that address one paper by intake id.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PaperIdParams {
    /// Catalog intake id of the paper.
    pub intake_id: i64,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for `library.show_paper_metadata_report`: the paper, and
/// optionally the profile to judge it under.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PaperAuditReadParams {
    /// Catalog intake id of the paper.
    pub intake_id: i64,
    /// Paper-side audit profile to grade under: `default`,
    /// `trust_source`, or `strict`. Omit to grade under the library's
    /// own profile overlay. Another name is refused.
    #[serde(default)]
    pub audit_profile: Option<String>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.show_toc` and `library.show_paper_toc`
/// tools.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShowTocParams {
    /// Catalog intake id of the book or paper.
    pub intake_id: i64,
    /// Number of leading TOC entries to skip. Pass a previous
    /// response's `next_offset` here to read the following page.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Maximum TOC entries in this page. Defaults to and is clamped
    /// by the server-side cap (2000); around 200 keeps responses
    /// small.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Return slim entries carrying only node_id, title, and depth —
    /// the cheap way to scan a book's structure. The node_id feeds
    /// library.read_span.
    #[serde(default)]
    pub titles_only: bool,
    /// Keep only entries at depth <= max_depth. The book root is
    /// depth 0, so 1 keeps the root plus its top-level divisions.
    #[serde(default)]
    pub max_depth: Option<i64>,
    /// Keep only entries whose title contains this substring,
    /// ignoring ASCII case. The cheap way to locate one chapter or
    /// section by name.
    #[serde(default)]
    pub title_substring: Option<String>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

impl ShowTocParams {
    /// The pagination, projection, and filter fields as the ops-side
    /// args.
    pub fn toc_args(&self) -> ShowTocArgs {
        ShowTocArgs {
            offset: self.offset.unwrap_or(0),
            limit: self.limit,
            titles_only: self.titles_only,
            max_depth: self.max_depth,
            title_substring: self.title_substring.clone(),
        }
    }
}

/// Arguments for the `library.read_context` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadContextParams {
    /// Corpus node id of the anchor leaf — take it from a search
    /// citation's `start_node_id` or from a passage of a previous
    /// read.
    pub node_id: i64,
    /// Which pipeline the anchor node belongs to (`book` for ingest,
    /// `paper` for glean). Defaults to `book` via
    /// [`ItemKind::default`] so existing callers stay green; pass
    /// `paper` to read context from the papers corpus.
    #[serde(default)]
    #[schemars(with = "String")]
    pub kind: ItemKind,
    /// Number of leaves to include before the anchor. Defaults to 3;
    /// clamped server-side.
    #[serde(default)]
    pub before: Option<u32>,
    /// Number of leaves to include after the anchor. Defaults to 3;
    /// clamped server-side.
    #[serde(default)]
    pub after: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused. Unrelated to `kind`.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.read_span` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadSpanParams {
    /// Corpus node id of the organizing node (chapter, section, ...)
    /// to read — take it from `library.show_toc`.
    pub node_id: i64,
    /// Which pipeline the target node belongs to (`book` for ingest,
    /// `paper` for glean). Defaults to `book` via
    /// [`ItemKind::default`] so existing callers stay green; pass
    /// `paper` to read from the papers corpus.
    #[serde(default)]
    #[schemars(with = "String")]
    pub kind: ItemKind,
    /// Resume cursor: the `next_offset` of the previous page. Omit to
    /// read from the span's start.
    #[serde(default)]
    pub start_after: Option<i64>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused. Unrelated to `kind`.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for `library.list_metadata` and
/// `library.list_paper_metadata`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListMetadataParams {
    /// Substring match against the title as extraction and enrichment
    /// wrote it, before any correction — the layer a review pass asks
    /// about. To search the titles rows report, use
    /// `library.find_books`.
    #[serde(default)]
    pub title_substring: Option<String>,
    /// Match the audit's row-level confidence against this set:
    /// `low`, `medium`, `high`. An unrecognised name is refused rather
    /// than dropped.
    #[serde(default)]
    pub confidence_in: Option<Vec<String>>,
    /// Match the review status against this set: `pending`,
    /// `approved`, `acknowledged`, `rejected`. A book never reviewed
    /// counts as `pending`. An unrecognised name is refused rather
    /// than dropped.
    #[serde(default)]
    pub review_status_in: Option<Vec<String>>,
    /// Maximum number of rows in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `reference.lookup` tool. `library` is
/// `Option<String>` to honour the same default-or-explicit semantics
/// used by every other read tool.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ReferenceLookupParams {
    pub library: Option<String>,
    /// Book slug to scope the lookup to. Pass `"*"` to query every
    /// registered book and rank cross-book hits by `authority_rank`.
    pub book: String,
    /// The lookup key; resolves directly against
    /// `reference_entries.entry_key`.
    pub entry_key: String,
    /// Optional payload-key whitelist. When `Some`, payload keys
    /// outside the list are stripped from each hit.
    pub fields: Option<Vec<String>>,
    /// Optional quality_flag-severity floor. When set, hits whose
    /// flags do not include at least one flag at or above this
    /// severity drop out. Recognised values: `ok`, `info`, `warn`,
    /// `error`.
    pub min_severity: Option<String>,
    /// Book slugs whose entries must not be returned. Only meaningful
    /// with `book = "*"`, where the lookup spans every registered
    /// book; passing it alongside a single-book scope is an error
    /// rather than a no-op. An excluded book is also skipped as a
    /// redirect target, so it cannot re-enter the result that way.
    pub exclude_books: Option<Vec<String>>,
}

impl ReferenceLookupParams {
    /// The request without the library selector, in the shape the
    /// reference read runs.
    pub fn to_lookup(&self) -> ReferenceLookup {
        ReferenceLookup {
            book: self.book.clone(),
            entry_key: self.entry_key.clone(),
            fields: self.fields.clone(),
            min_severity: self.min_severity.clone(),
            exclude_books: self.exclude_books.clone(),
        }
    }
}

/// Arguments for the `reference.overlay_set` tool. `library` is
/// required for write tools, so this carries a bare `String` rather
/// than `Option<String>`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ReferenceOverlaySetParams {
    pub library: String,
    pub book_slug: String,
    pub entry_key: String,
    /// JSON object whose keys must be present in the property
    /// catalog (`crates/distill/data/property_catalog.toml`).
    pub overlay: serde_json::Value,
    /// Free-text edit summary recorded on the overlay row.
    pub reason: String,
}

impl ReferenceOverlaySetParams {
    /// The edit without the library selector, in the shape the
    /// reference write takes.
    pub fn to_request(&self) -> ReferenceOverlaySetRequest {
        ReferenceOverlaySetRequest {
            book_slug: self.book_slug.clone(),
            entry_key: self.entry_key.clone(),
            overlay: self.overlay.clone(),
            reason: self.reason.clone(),
        }
    }
}

/// Arguments for the control plane's `metadata.set`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataSetParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// The field to set. Must be a curator-editable bibliographic
    /// field; an unknown name is rejected and the error carries the
    /// full editable list.
    pub field: String,
    /// The new value.
    pub value: String,
    /// Why this value is correct, recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// Whether the value was checked against the source itself. The
    /// audit grades a confirmed override strong unless a validation
    /// check fails.
    #[serde(default)]
    pub confirmed: bool,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `metadata.clear`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataClearParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// The field whose override should be removed. A name outside the
    /// editable set is accepted only when an override row with that key
    /// exists (cleanup of stale rows); otherwise it is rejected.
    pub field: String,
    /// Why the override is being removed, recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `metadata.void`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataVoidParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// The field whose extracted value should be suppressed. Must be a
    /// curator-editable bibliographic field.
    pub field: String,
    /// Why the extracted value is wrong, recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `metadata.reaudit`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataReauditParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// Book-side audit profile to grade under: `default`,
    /// `trust_source`, or `strict`. Omit to grade under the library's
    /// own profile overlay. Another name is refused.
    #[serde(default)]
    pub audit_profile: Option<String>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `metadata.contributor_add`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataContributorAddParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// Contribution role: `author`, `translator`, `editor`, or `other`.
    pub role: String,
    /// The contributor's name, exactly as it should be searchable.
    pub name: String,
    /// The contributor's nationality, when known.
    #[serde(default)]
    pub nationality: Option<String>,
    /// Why this attribution is correct, recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `metadata.contributor_remove`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataContributorRemoveParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// Surrogate id of the contributor row, as listed by
    /// `library.show_book`.
    pub contributor_id: i64,
    /// Why the attribution is wrong, recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `metadata.ack`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataAckParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// Why the gap is being acknowledged; recorded on the audit row.
    pub reason: String,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `metadata.approve`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataApproveParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// Optional reason recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the control plane's `metadata.reject`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataRejectParams {
    /// Catalog intake id of the book.
    pub book: i64,
    /// Why the book is being rejected; recorded on the audit row.
    pub reason: String,
    /// Library short name from the registry. Omit to let the daemon
    /// route the call: the registry default when it is mounted, else
    /// the only mounted library; otherwise the call is refused.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for `library.metadata.set`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataSetParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field to set. Must be a curator-editable bibliographic
    /// field; an unknown name is rejected and the error carries the
    /// full editable list.
    pub field: String,
    /// The new value.
    pub value: String,
    /// Why this value is correct (e.g. the source consulted). Recorded
    /// on the audit row; required so an LLM edit always carries its
    /// justification.
    pub reason: String,
    /// Set true only after checking the value against the source
    /// itself (e.g. the copyright page via `library.read_span`), not
    /// against an external catalog alone. The audit grades a confirmed
    /// override strong unless a validation check fails. Defaults to
    /// false.
    pub confirmed: Option<bool>,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.clear`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataClearParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field whose override should be removed. A name outside the
    /// editable set is accepted only when an override row with that key
    /// exists (cleanup of stale rows); otherwise it is rejected.
    pub field: String,
    /// Why the override is being removed. Recorded on the audit row;
    /// required so an LLM edit always carries its justification.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.void`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataVoidParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field whose extracted value should be suppressed. Must be a
    /// curator-editable bibliographic field.
    pub field: String,
    /// Why the extracted value is wrong. Recorded on the audit row;
    /// required so an LLM edit always carries its justification.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.contributor_add`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataContributorAddParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Contribution role: `author`, `translator`, `editor`, or `other`.
    pub role: String,
    /// The contributor's name, exactly as it should be searchable.
    pub name: String,
    /// The contributor's nationality, when known.
    #[serde(default)]
    pub nationality: Option<String>,
    /// Why this attribution is correct (e.g. the source consulted).
    /// Recorded on the audit row; required so an LLM edit always
    /// carries its justification.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.contributor_remove`. Requires
/// `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataContributorRemoveParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Surrogate id of the contributor row, as listed by
    /// `library.show_book`.
    pub contributor_id: i64,
    /// Why the attribution is wrong. Recorded on the audit row;
    /// required so an LLM edit always carries its justification.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.reaudit`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataReauditParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.ack`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataAckParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Why the gap is being acknowledged; recorded on the audit row.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.approve`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataApproveParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Optional reason recorded on the audit row.
    #[serde(default)]
    pub reason: Option<String>,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.reject`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LibraryMetadataRejectParams {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Why the book is being rejected; recorded on the audit row.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

routed_by_optional_library!(
    LibraryOnlyParams,
    SearchParams,
    SearchInBookParams,
    SearchInPaperParams,
    ListBooksParams,
    ListPapersParams,
    PageParams,
    FindBooksParams,
    FindPapersParams,
    BookIdParams,
    PaperIdParams,
    PaperAuditReadParams,
    ShowTocParams,
    ReadContextParams,
    ReadSpanParams,
    ListMetadataParams,
    ReferenceLookupParams,
    MetadataSetParams,
    MetadataClearParams,
    MetadataVoidParams,
    MetadataReauditParams,
    MetadataContributorAddParams,
    MetadataContributorRemoveParams,
    MetadataAckParams,
    MetadataApproveParams,
    MetadataRejectParams,
);

routed_by_required_library!(
    ReferenceOverlaySetParams,
    LibraryMetadataSetParams,
    LibraryMetadataClearParams,
    LibraryMetadataVoidParams,
    LibraryMetadataContributorAddParams,
    LibraryMetadataContributorRemoveParams,
    LibraryMetadataReauditParams,
    LibraryMetadataAckParams,
    LibraryMetadataApproveParams,
    LibraryMetadataRejectParams,
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).expect("the params deserialize")
    }

    /// `awaiting_text` is optional on both transports: absent and
    /// `null` filter nothing, and each boolean is taken as given.
    #[test]
    fn awaiting_text_accepts_absent_null_and_both_booleans() {
        for (body, expected) in [
            (json!({}), None),
            (json!({ "awaiting_text": null }), None),
            (json!({ "awaiting_text": false }), Some(false)),
            (json!({ "awaiting_text": true }), Some(true)),
        ] {
            let p: FindBooksParams = parse(body.clone());
            assert_eq!(p.awaiting_text, expected, "{body}");
        }
    }

    /// An anchor or span read without `kind` reads the book corpus,
    /// and `paper` selects the paper corpus.
    #[test]
    fn a_read_kind_defaults_to_book_and_takes_paper() {
        let context: ReadContextParams = parse(json!({ "node_id": 42 }));
        assert_eq!(context.kind, ItemKind::Book);
        let context: ReadContextParams = parse(json!({ "node_id": 42, "kind": "paper" }));
        assert_eq!(context.kind, ItemKind::Paper);
        let span: ReadSpanParams = parse(json!({ "node_id": 7 }));
        assert_eq!(span.kind, ItemKind::Book);
        let span: ReadSpanParams = parse(json!({ "node_id": 7, "kind": "paper" }));
        assert_eq!(span.kind, ItemKind::Paper);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let p: BookIdParams = parse(json!({
            "intake_id": 42,
            "library": "demo",
            "extra_field": "ignored",
        }));
        assert_eq!(p.intake_id, 42);
        assert_eq!(p.library(), Some("demo"));
    }

    #[test]
    fn search_overrides_project_each_sides_exclusions_separately() {
        let p: SearchParams = parse(json!({
            "query": "q",
            "kind": "all",
            "bypass_index": true,
            "nprobes": 8,
            "refine_factor": 3,
            "exclude_book_intake_ids": [2, 5],
            "exclude_paper_intake_ids": [9],
        }));
        let books = p.overrides_excluding(&p.exclude_book_intake_ids);
        assert!(books.bypass_index);
        assert_eq!(books.nprobes, Some(8));
        assert_eq!(books.refine_factor, Some(3));
        assert_eq!(
            books.exclude_partitions,
            vec![
                bookrack_core::PartitionIdx::new(2),
                bookrack_core::PartitionIdx::new(5)
            ]
        );
        let papers = p.overrides_excluding(&p.exclude_paper_intake_ids);
        assert_eq!(
            papers.exclude_partitions,
            vec![bookrack_core::PartitionIdx::new(9)]
        );
    }

    #[test]
    fn a_search_kind_refuses_the_other_sides_exclusion_list() {
        for (kind, field) in [
            ("book", "exclude_paper_intake_ids"),
            ("paper", "exclude_book_intake_ids"),
        ] {
            let p: SearchParams = parse(json!({ "query": "q", "kind": kind, field: [9] }));
            let err = p
                .checked_kind()
                .expect_err("the mismatched list is refused");
            let message = format!("{err}");
            assert!(
                message.contains(field) && message.contains(kind),
                "the refusal must name the field and the kind: {message}"
            );
        }
        let p: SearchParams = parse(json!({
            "query": "q",
            "kind": "all",
            "exclude_book_intake_ids": [2],
            "exclude_paper_intake_ids": [9],
        }));
        assert_eq!(p.checked_kind().expect("admissible"), "all");
        let p: SearchParams = parse(json!({ "query": "q" }));
        assert_eq!(p.checked_kind().expect("admissible"), "book");
    }

    /// A write that requires its selector reports it as given; the
    /// read selectors report an absent one as `None`.
    #[test]
    fn the_selector_is_read_from_the_key_each_type_declares() {
        let write: LibraryMetadataAckParams = parse(json!({
            "intake_id": 1, "reason": "r", "library": "alpha",
        }));
        assert_eq!(write.library(), Some("alpha"));
        let info: LibraryInfoParams = parse(json!({ "name": "beta" }));
        assert_eq!(info.library(), Some("beta"));
        assert_eq!(LibraryInfoParams::LIBRARY_KEY, "name");
        let read: LibraryOnlyParams = parse(json!({}));
        assert_eq!(read.library(), None);
    }
}
