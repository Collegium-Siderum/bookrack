// SPDX-License-Identifier: Apache-2.0

//! The rows of [`LIBRARY_METHODS`].

use bookrack_refs::LookupResult;

use super::params::*;
use super::{
    Access, MethodSpec, Output, RegionKind, RoutedParams, Selection, input_schema, output_schema,
};
use crate::dto::categories::CategoryCounts;
use crate::dto::info::LibraryInfo;
use crate::dto::metadata_report::MetadataListPage;
use crate::dto::vectors_status::VectorsStatus;
use crate::dto::writes::{AddContributorOutcome, ReauditOutcome, WriteOutcome};
use crate::dto::{LibraryStats, ListBooksResult, ListPapersResult};
use crate::writes::reference::ReferenceOverlaySetResult;

/// One row. The selection keys are read from the params types'
/// [`RoutedParams`] impls, so a row cannot name a key its params do not
/// carry.
macro_rules! method {
    (
        $name:literal => $mcp:expr, $access:expr,
        $params:ty => $mcp_params:ty, $output:expr, $summary:expr $(,)?
    ) => {
        MethodSpec {
            name: $name,
            mcp_name: $mcp,
            summary: $summary,
            access: $access,
            selection: Selection::Routed {
                control_key: <$params as RoutedParams>::LIBRARY_KEY,
                mcp_key: <$mcp_params as RoutedParams>::LIBRARY_KEY,
            },
            needs_queue: false,
            input_schema: input_schema::<$params>,
            mcp_input_schema: input_schema::<$mcp_params>,
            output: $output,
        }
    };
    ($name:literal, $access:expr, $params:ty, $output:expr, $summary:expr $(,)?) => {
        method!($name => Some($name), $access, $params => $params, $output, $summary)
    };
}

const READ: Access = Access::Read;
const CURATION: Access = Access::Write {
    region: RegionKind::Curation,
};

/// Every method that acts on one library: the library reads, the
/// reference book's lookup and overlay edit, and the book-side
/// metadata writes.
pub const LIBRARY_METHODS: &[MethodSpec] = &[
    method!(
        "library.info" => Some("library.info"),
        READ,
        LibraryInfoParams => LibraryOnlyParams,
        Output::Structured(output_schema::<LibraryInfo>),
        "Return a one-page status card for the open library: schema \
         versions, embedder configuration, stamped index parameters, \
         live row count, intake counts, and approximate disk usage.",
    ),
    method!(
        "library.stats",
        READ,
        LibraryOnlyParams,
        Output::Structured(output_schema::<LibraryStats>),
        "Return aggregate counts over the library: intakes by status \
         and format, book states by pipeline stage, and retrieval \
         issues by triage status.",
    ),
    method!(
        "library.categories",
        READ,
        LibraryOnlyParams,
        Output::Structured(output_schema::<CategoryCounts>),
        "Count the books under each category tag, most-used first, plus \
         the books carrying no tag. Read this before passing \
         `categories` to library.find_books: it is where this \
         library's tag vocabulary can be seen, and a library nobody \
         has tagged reports every book as uncategorised.",
    ),
    method!(
        "library.list_books",
        READ,
        ListBooksParams,
        Output::Structured(output_schema::<ListBooksResult>),
        "List books known to the library, paginated. Returns a slice \
         of book summaries plus the total matching count and a \
         truncated flag. Each summary carries source_filename, the \
         basename of the file the book was ingested from, which \
         identifies a book whose title is missing or is an export-tool \
         placeholder.",
    ),
    method!(
        "library.find_books",
        READ,
        FindBooksParams,
        Output::Structured(output_schema::<ListBooksResult>),
        "Search the book registry by title substring (fuzzy), contributor \
         name (exact), file format, or category tags. The `categories` \
         filter matches books carrying at least one of the listed tags; \
         library.categories lists the tags this library carries. \
         `awaiting_text` lists the scan sources still without text.",
    ),
    method!(
        "library.show_book",
        READ,
        BookIdParams,
        Output::NullableObject,
        "Fetch the full bibliographic record for one book by intake id. \
         Returns effective biblio attributes, the active overrides \
         (which fields are curated rather than extracted, by whom and \
         when; `value: null` marks a suppressed extracted value), the \
         contributor list, the source-side record of the ingested file \
         (source_path / source_filename / source_sha256 / intake_at / \
         page_count / byte_size; source_path is recorded verbatim at \
         intake, so it may be relative or no longer exist on disk), and \
         toc_stats (entry_count / max_depth of the ingested TOC; null \
         when nothing is ingested) — check toc_stats before \
         library.show_toc to pick a pagination or projection strategy. \
         Returns null when no such book is registered.",
    ),
    method!(
        "library.show_toc",
        READ,
        ShowTocParams,
        Output::NullableObject,
        "Return the table of contents of one book: a depth-first list \
         of organizing nodes (chapters, sections, ...) with their \
         titles, depths, and document-order spans, paginated. To \
         continue, pass the returned next_offset back as offset until \
         next_offset is null; total counts every matching entry. A \
         limit of about 200 keeps responses small. To scan structure, \
         prefer titles_only (each entry keeps node_id / title / depth; \
         node_id is what library.read_span takes) and max_depth (1 = \
         root plus top-level divisions). To locate one chapter or \
         section by name, filter with title_substring instead of \
         paging the whole TOC. Returns null when no such book is \
         ingested.",
    ),
    method!(
        "library.read_context",
        READ,
        ReadContextParams,
        Output::NullableObject,
        "Read the passages surrounding one anchor passage, in document \
         order: N leaves before and N after, paragraph-precise. Pass the \
         node_id from a search citation's start_node_id or from a \
         passage of a previous read. Returns null when no such node \
         exists; rejects organizing nodes (read those with \
         library.read_span).",
    ),
    method!(
        "library.read_span",
        READ,
        ReadSpanParams,
        Output::NullableObject,
        "Read the full text under one TOC node (chapter, section, ...) \
         in document order, paginated by a character budget. Pass the \
         node_id from library.show_toc; to continue, pass the returned \
         next_offset back as start_after until next_offset is null. \
         Returns null when no such node exists; rejects leaf nodes \
         (read around those with library.read_context).",
    ),
    method!(
        "library.show_metadata_audit",
        READ,
        BookIdParams,
        Output::NullableObject,
        "Return the metadata-status record for one book: the \
         bibliographic detail plus the persisted audit verdict, \
         confidence, and current review status. Returns null when \
         no such book is registered.",
    ),
    method!(
        "library.show_metadata_report",
        READ,
        BookIdParams,
        Output::NullableObject,
        "Recompute the metadata plausibility audit for one book from \
         its cached extraction and return the full per-field report: \
         origin (extracted / override / override_confirmed / voided), \
         grade, flags, and hint per field, plus TOC shape flags and \
         copyright-page block candidates, next to the stored verdict / \
         confidence rollup for comparison. Use it to see why a book is \
         `needs_work` / `low` before editing. Grades under the \
         library's audit profile and audit data overlays, as \
         library.metadata.reaudit does; nothing is written back — \
         library.metadata.reaudit is the write path that refreshes the \
         stored rollup. Returns null when no such book is registered.",
    ),
    method!(
        "library.list_metadata",
        READ,
        ListMetadataParams,
        Output::Structured(output_schema::<MetadataListPage>),
        "List registered books with their current confidence and review \
         status, regardless of audit verdict. Filters match the metadata as \
         extracted, before curation, so this is the tool for finding records \
         that need fixing; `library.find_books` searches the corrected \
         values. Paginated.",
    ),
    method!(
        "library.list_pending_reviews",
        READ,
        PageParams,
        Output::Structured(output_schema::<MetadataListPage>),
        "List books whose metadata audit confidence is low or medium \
         and whose review is still pending or acknowledged. Paginated.",
    ),
    method!(
        "library.show_audit_trail",
        READ,
        BookIdParams,
        Output::TextOnly,
        "Return the metadata-edit audit trail for one book, oldest \
         first. Returns null when no such book is registered.",
    ),
    method!(
        "library.show_pipeline_trail",
        READ,
        BookIdParams,
        Output::TextOnly,
        "Return the book-level pipeline audit trail for one book, \
         oldest first. Each row records a pipeline sub-step (stage, \
         outcome, duration, run id). Returns null when no such book \
         is registered.",
    ),
    method!(
        "library.list_papers",
        READ,
        ListPapersParams,
        Output::Structured(output_schema::<ListPapersResult>),
        "List papers known to the library, paginated. Mirrors \
         library.list_books for the paper pipeline. Returns paper \
         summaries plus the total matching count and a truncated flag. \
         Each summary carries source_filename, the basename of the file \
         the paper was ingested from, which identifies a paper whose \
         title the identify pass did not extract.",
    ),
    method!(
        "library.find_papers",
        READ,
        FindPapersParams,
        Output::Structured(output_schema::<ListPapersResult>),
        "Search the paper registry by title substring (fuzzy), \
         contributor name (exact), year (exact), venue substring \
         (matched against container title), or DOI (exact). Mirrors \
         library.find_books for the paper pipeline; rows carry \
         source_filename like library.list_papers.",
    ),
    method!(
        "library.show_paper",
        READ,
        PaperIdParams,
        Output::NullableObject,
        "Fetch the full bibliographic record for one paper by intake \
         id. Mirrors library.show_book for the paper pipeline, \
         toc_stats included; the abstract text is in the detail \
         response, not in list summaries. Also returns the source-side \
         record of the ingested file (source_path / source_filename / \
         source_sha256 / intake_at / page_count / byte_size; \
         source_path is recorded verbatim at intake, so it may be \
         relative or no longer exist on disk — papers.fetch_source \
         locates the archived copy instead). Returns null when no such \
         paper is registered.",
    ),
    method!(
        "library.show_paper_toc",
        READ,
        ShowTocParams,
        Output::NullableObject,
        "Return the table of contents of one paper. Papers carry one \
         Work root plus one prose leaf, so the TOC is effectively \
         empty for a well-formed paper. The shape, the offset / limit \
         / next_offset pagination contract, and the titles_only / \
         max_depth / title_substring parameters mirror \
         library.show_toc for the book pipeline.",
    ),
    method!(
        "library.show_paper_metadata_report",
        READ,
        PaperAuditReadParams,
        Output::NullableObject,
        "Recompute the metadata plausibility audit for one paper from \
         its cached extraction and return the full per-field report: \
         origin (extracted / override / override_confirmed / voided), \
         grade, flags, and hint per field, plus the cross-field flags \
         and the CSL type the required-field matrix was selected by. \
         Each response also carries the judgement stored on the paper's \
         audit row; the two disagreeing means the paper was edited after \
         that judgement was made. Grades under the library's paper audit \
         profile and audit data overlays, or under the built-in profile \
         `audit_profile` names; nothing is written back. No tool writes \
         paper metadata: the operator's `bookrack papers metadata \
         reaudit <intake_id>` brings the stored row back in line. \
         Returns null when no such paper is registered.",
    ),
    method!(
        "library.show_paper_audit_trail",
        READ,
        PaperIdParams,
        Output::TextOnly,
        "Return the metadata-edit audit trail for one paper, oldest \
         first: who changed which field, what it said before, and why. \
         Returns null when no such paper is registered and no edits \
         were ever recorded against the id.",
    ),
    method!(
        "library.list_paper_metadata",
        READ,
        ListMetadataParams,
        Output::Structured(output_schema::<MetadataListPage>),
        "List registered papers with their current confidence and \
         review status, regardless of audit verdict. Filters match \
         the metadata as extracted, before curation, so this is the \
         tool for finding records that need fixing; \
         `library.find_papers` searches the corrected values. \
         Paginated.",
    ),
    method!(
        "library.list_paper_pending_reviews",
        READ,
        PageParams,
        Output::Structured(output_schema::<MetadataListPage>),
        "List papers whose metadata audit confidence is low or medium \
         and whose review is still pending or acknowledged. Paginated.",
    ),
    method!(
        "papers.export_csl" => None,
        READ,
        PaperIdParams => PaperIdParams,
        Output::TextOnly,
        "Return one paper's bibliographic record as a CSL-JSON item. \
         Returns null when no such paper is registered.",
    ),
    method!(
        "papers.fetch_source",
        READ,
        PaperIdParams,
        Output::NullableObject,
        "Return the absolute path, byte size, and SHA-256 of one \
         paper's archived source PDF. The client opens the path \
         with its own file-system primitives — the bytes do not \
         flow through the control plane. Returns null when the \
         intake id is unknown; raises an error when the intake \
         exists but its source PDF was not archived (e.g. glean \
         ran with `keep_source_pdf = false`).",
    ),
    method!(
        "library.search",
        READ,
        SearchParams,
        Output::TextOnly,
        "Search the library for passages relevant to a natural-language \
         query. Returns cited passages, nearest first, each with a \
         breadcrumb trail and source location. `kind` selects which \
         side: `\"book\"` (the default; existing behaviour), `\"paper\"` \
         (only the paper-side store), or `\"all\"` (both stores, merged \
         by ascending distance). To suppress books you have already \
         read or judged irrelevant, pass their intake ids in \
         `exclude_book_intake_ids` (or `exclude_paper_intake_ids` for \
         papers); the ids must match the `kind` being searched.",
    ),
    method!(
        "library.search_in_book",
        READ,
        SearchInBookParams,
        Output::TextOnly,
        "Search a single book for passages relevant to a query. Pass \
         the book's intake_id (from library.list_books / \
         library.show_book). Returns cited passages, nearest first.",
    ),
    method!(
        "library.search_in_paper",
        READ,
        SearchInPaperParams,
        Output::TextOnly,
        "Search a single paper for passages relevant to a query. Pass \
         the paper's intake_id (from library.list_papers / \
         library.show_paper). Returns cited passages, nearest first.",
    ),
    method!(
        "library.vectors_status",
        READ,
        LibraryOnlyParams,
        Output::Structured(output_schema::<VectorsStatus>),
        "Snapshot the vector store: chunk-table row count, every ANN \
         index LanceDB enumerates with its per-shard statistics, the \
         persisted ANN config, and any drift between the on-disk meta \
         and the indices LanceDB actually carries.",
    ),
    method!(
        "reference.lookup",
        READ,
        ReferenceLookupParams,
        Output::Structured(output_schema::<LookupResult>),
        "Look up a reference-book entry by its normalized key. \
         `book` is either a specific book slug or `*` to query \
         every registered book and rank cross-book hits by \
         authority_rank. `fields` optionally restricts which \
         payload keys appear on each hit; `min_severity` \
         (ok | info | warn | error) drops hits whose quality_flags \
         carry no concern at or above that severity. Single-hit \
         redirect entries auto-follow one hop.",
    ),
    method!(
        "reference.overlay_set",
        Access::Write {
            region: RegionKind::Reference
        },
        ReferenceOverlaySetParams,
        Output::Structured(output_schema::<ReferenceOverlaySetResult>),
        "Layer a user edit on top of one reference entry. Every key \
         in `overlay` must be in property_catalog.toml. `reason` is \
         a free-text edit summary recorded on the overlay row. The \
         `edited_at` stamp is set to the daemon's current UTC time.",
    ),
    method!(
        "metadata.set" => Some("library.metadata.set"),
        CURATION,
        MetadataSetParams => LibraryMetadataSetParams,
        Output::Structured(output_schema::<WriteOutcome>),
        "Set an override on one editable bibliographic field of one \
         book. An unknown field name is rejected with the editable \
         list in the error. The extracted value is preserved; the \
         override wins on read. Appends one audit row tagged \
         `actor_kind=llm` carrying the required `reason`. Pass \
         `confirmed: true` only after verifying the value against \
         the source itself; the audit then grades the field strong.",
    ),
    method!(
        "metadata.clear" => Some("library.metadata.clear"),
        CURATION,
        MetadataClearParams => LibraryMetadataClearParams,
        Output::Structured(output_schema::<WriteOutcome>),
        "Remove an override on one bibliographic field of one book, \
         reverting to the extracted value. An editable field with no \
         override still appends an audit row recording the attempt; \
         an unknown field name is rejected unless a stale override \
         row with that key exists. The required `reason` lands on \
         the audit row.",
    ),
    method!(
        "metadata.void" => Some("library.metadata.void"),
        CURATION,
        MetadataVoidParams => LibraryMetadataVoidParams,
        Output::Structured(output_schema::<WriteOutcome>),
        "Suppress one field's extracted value without supplying a \
         replacement: the field reads as absent until a correct \
         value is set. For extracted values known to be wrong when \
         no right value is at hand. `library.metadata.clear` \
         removes the suppression. Appends one audit row tagged \
         `actor_kind=llm` carrying the required `reason`.",
    ),
    method!(
        "metadata.reaudit" => Some("library.metadata.reaudit"),
        CURATION,
        MetadataReauditParams => LibraryMetadataReauditParams,
        Output::Structured(output_schema::<ReauditOutcome>),
        "Re-run the metadata plausibility audit for one book from \
         its cached extraction, refreshing the stored verdict and \
         confidence so they reflect the current effective metadata \
         (overrides included). Use after correcting fields so the \
         stored `needs_work` / `low` can catch up. Grades under the \
         library's audit profile and audit data overlays; the review \
         status is untouched. Returns the previous and new verdict / \
         confidence pair.",
    ),
    method!(
        "metadata.contributor_add" => Some("library.metadata.contributor_add"),
        CURATION,
        MetadataContributorAddParams => LibraryMetadataContributorAddParams,
        Output::Structured(output_schema::<AddContributorOutcome>),
        "Attribute a contributor (role: author / translator / editor \
         / other) to one book. The row is written with origin `user`, \
         survives re-ingest, and is immediately searchable via \
         `library.find_books` `contributor_name`. Appends one audit \
         row tagged `actor_kind=llm` carrying the required `reason`. \
         Returns the new row's `contributor_id`.",
    ),
    method!(
        "metadata.contributor_remove" => Some("library.metadata.contributor_remove"),
        CURATION,
        MetadataContributorRemoveParams => LibraryMetadataContributorRemoveParams,
        Output::Structured(output_schema::<WriteOutcome>),
        "Remove one contributor row from one book by the \
         `contributor_id` listed in `library.show_book`, whatever \
         its origin — the path for stripping a wrong extracted \
         attribution (e.g. an ebook packager credited as the \
         author). Appends one audit row tagged `actor_kind=llm` \
         carrying the required `reason`. A forced re-ingest \
         re-seeds extracted rows, so such a removal may need \
         repeating after one.",
    ),
    method!(
        "metadata.ack" => Some("library.metadata.ack"),
        CURATION,
        MetadataAckParams => LibraryMetadataAckParams,
        Output::Structured(output_schema::<WriteOutcome>),
        "Acknowledge a metadata gap on one book: the audit verdict \
         is unchanged; the review row is flipped to `acknowledged` \
         with the given reason. The book stays on the review queue \
         until approved or rejected.",
    ),
    method!(
        "metadata.approve" => Some("library.metadata.approve"),
        CURATION,
        MetadataApproveParams => LibraryMetadataApproveParams,
        Output::Structured(output_schema::<WriteOutcome>),
        "Approve the metadata record on one book: assert that the \
         effective metadata matches the source. Flips the review \
         row to `approved`. The audit verdict is unchanged.",
    ),
    method!(
        "metadata.reject" => Some("library.metadata.reject"),
        CURATION,
        MetadataRejectParams => LibraryMetadataRejectParams,
        Output::Structured(output_schema::<WriteOutcome>),
        "Reject one book: flip the review row to `rejected`. \
         Pipeline rows stay in place so downstream consumers can \
         filter on `rejected`. Records the reason on the audit row.",
    ),
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::{Map, Value};

    use super::super::{Access, LIBRARY_METHODS, MethodSpec, Output, Selection, library_tool};

    fn mcp_rows() -> impl Iterator<Item = &'static MethodSpec> {
        LIBRARY_METHODS
            .iter()
            .filter(|spec| spec.mcp_name.is_some())
    }

    fn properties(schema: &Map<String, Value>) -> &Map<String, Value> {
        schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("an object schema with properties: {schema:?}"))
    }

    /// The table holds the 29 library reads, the reference lookup and
    /// overlay edit, and the nine book-side metadata writes; every name
    /// is unique on its surface, and only `papers.export_csl` is left
    /// off MCP.
    #[test]
    fn the_table_holds_forty_methods_with_unique_names() {
        assert_eq!(LIBRARY_METHODS.len(), 40);
        let reads = LIBRARY_METHODS
            .iter()
            .filter(|spec| spec.access == Access::Read)
            .count();
        assert_eq!(reads, 30, "29 library reads and the reference lookup");

        let names: HashSet<_> = LIBRARY_METHODS.iter().map(|spec| spec.name).collect();
        assert_eq!(names.len(), LIBRARY_METHODS.len(), "a control name repeats");
        let tools: HashSet<_> = mcp_rows().filter_map(|spec| spec.mcp_name).collect();
        assert_eq!(tools.len(), mcp_rows().count(), "an MCP name repeats");
        let off_mcp: Vec<_> = LIBRARY_METHODS
            .iter()
            .filter(|spec| spec.mcp_name.is_none())
            .map(|spec| spec.name)
            .collect();
        assert_eq!(off_mcp, ["papers.export_csl"]);
        for spec in LIBRARY_METHODS {
            assert!(
                !spec.summary.trim().is_empty(),
                "{} has no summary",
                spec.name
            );
            assert!(!spec.needs_queue, "{} runs without the queue", spec.name);
        }
    }

    /// Every input schema is an object at the root, carries neither a
    /// root `title` (which would only name the Rust type) nor a root
    /// `description` (which would only repeat its doc comment), and
    /// lists the key the row says the library is selected by.
    #[test]
    fn every_input_schema_is_an_object_carrying_the_selection_key() {
        for spec in LIBRARY_METHODS {
            let Selection::Routed {
                control_key,
                mcp_key,
            } = spec.selection
            else {
                panic!("{} acts on one library", spec.name);
            };
            for (label, schema, key) in [
                ("control", (spec.input_schema)(), control_key),
                ("mcp", (spec.mcp_input_schema)(), mcp_key),
            ] {
                assert_eq!(
                    schema["type"], "object",
                    "{} {label}: {schema:?}",
                    spec.name
                );
                for root_key in ["title", "description"] {
                    assert!(
                        !schema.contains_key(root_key),
                        "{} {label} carries a root {root_key}: {schema:?}",
                        spec.name
                    );
                }
                assert!(
                    properties(&schema).contains_key(key),
                    "{} {label} names no {key:?}: {schema:?}",
                    spec.name
                );
            }
        }
    }

    /// A structured output is an object at the root; the shapes split
    /// as MCP carries them: 23 object roots, 10 objects that may be
    /// `null`, and the 3 searches and 3 trails as text alone.
    #[test]
    fn the_outputs_split_by_the_shape_mcp_carries() {
        let (mut structured, mut nullable, mut text) = (0, 0, 0);
        for spec in mcp_rows() {
            match spec.output {
                Output::Structured(schema) => {
                    let schema = schema();
                    assert_eq!(schema["type"], "object", "{}: {schema:?}", spec.name);
                    for root_key in ["title", "description"] {
                        assert!(!schema.contains_key(root_key), "{}: {schema:?}", spec.name);
                    }
                    structured += 1;
                }
                Output::NullableObject => nullable += 1,
                Output::TextOnly => text += 1,
            }
        }
        assert_eq!((structured, nullable, text), (23, 10, 6));
    }

    /// The pending-review lists take a page and a library, and none of
    /// the three filters they never applied.
    #[test]
    fn the_review_queues_take_no_filters_they_ignore() {
        for tool in [
            "library.list_pending_reviews",
            "library.list_paper_pending_reviews",
        ] {
            let spec = library_tool(tool).expect("a table row");
            let schema = (spec.mcp_input_schema)();
            let keys: HashSet<_> = properties(&schema).keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                HashSet::from(["limit", "offset", "library"]),
                "{tool}"
            );
        }
    }

    /// The report reads say which rules they grade under: the library's
    /// own overlays, not a default profile they no longer run.
    #[test]
    fn the_report_reads_describe_the_rules_they_grade_under() {
        for tool in [
            "library.show_metadata_report",
            "library.show_paper_metadata_report",
        ] {
            let spec = library_tool(tool).expect("a table row");
            assert!(
                !spec.summary.contains("default audit profile"),
                "{tool}: {}",
                spec.summary
            );
            assert!(
                spec.summary.contains("overlays"),
                "{tool}: {}",
                spec.summary
            );
        }
        let paper = library_tool("library.show_paper_metadata_report").expect("a table row");
        assert!(properties(&(paper.mcp_input_schema)()).contains_key("audit_profile"));
    }
}
