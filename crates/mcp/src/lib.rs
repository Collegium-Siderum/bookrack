// SPDX-License-Identifier: Apache-2.0

//! bookrack MCP server: exposes the shared ops layer to agent clients
//! over streamable HTTP.
//!
//! The server is a thin shell. It holds the warm [`LibraryRegistry`]
//! behind an `Arc` and routes every tool call through it — the
//! tool's `library` selector picks the target handle, or falls back
//! to the registry's current default when absent. The crate depends
//! only on `bookrack-ops`, never on the database crates behind it, so
//! a schema change downstream leaves it untouched.

use std::sync::Arc;

use anyhow::Context;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::dto::BookFilter;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{OpsError, SearchOptions, reads, with_source_override, writes};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Arguments for the `library.search` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
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
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.search_in_book` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchInBookArgs {
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
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

impl SearchArgs {
    /// Project the override fields onto the underlying [`SearchOptions`]
    /// struct the ops layer consumes.
    fn overrides(&self) -> SearchOptions {
        SearchOptions {
            bypass_index: self.bypass_index,
            nprobes: self.nprobes,
            refine_factor: self.refine_factor,
        }
    }
}

impl SearchInBookArgs {
    /// Project the override fields onto the underlying [`SearchOptions`]
    /// struct the ops layer consumes.
    fn overrides(&self) -> SearchOptions {
        SearchOptions {
            bypass_index: self.bypass_index,
            nprobes: self.nprobes,
            refine_factor: self.refine_factor,
        }
    }
}

/// Arguments for the `library.list_books` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListBooksArgs {
    /// Maximum number of books in this page. Server-side cap applies.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.find_books` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindBooksArgs {
    /// Substring match against the book title.
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
    /// Exact-equality match against the file format (`epub`, `pdf`).
    #[serde(default)]
    pub format: Option<String>,
    /// Reserved hook for category-based filtering. Accepted on the
    /// wire today but not honoured server-side; will be enabled in a
    /// future release.
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// Maximum number of books in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for the `library.show_book` and `library.show_toc` tools.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BookIdArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments shared by `library.list_metadata` and
/// `library.list_pending_reviews`. Identical field set — the two tools
/// differ only in which catalog rows they include.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataPageArgs {
    /// Maximum number of rows in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Number of leading rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for `library.stats`. Carries only the library selector;
/// the tool itself accepts no other inputs.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct LibraryOnlyArgs {
    /// Library short name from the registry. Omit to target the
    /// session's default library.
    #[serde(default)]
    pub library: Option<String>,
}

/// Arguments for `library.metadata.set`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataSetArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field to set (`title`, `publisher`, `year`, `language`, ...).
    pub field: String,
    /// The new value.
    pub value: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.clear`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataClearArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// The field whose override should be removed.
    pub field: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// Arguments for `library.metadata.ack`. Requires `library`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MetadataAckArgs {
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
pub struct MetadataApproveArgs {
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
pub struct MetadataRejectArgs {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Why the book is being rejected; recorded on the audit row.
    pub reason: String,
    /// Library short name from the registry. Write tools require an
    /// explicit selector so a misrouted call never silently lands on
    /// the wrong library's catalog.
    pub library: String,
}

/// MCP request handler. The streamable-HTTP service clones it per
/// session; the heavy state sits behind an `Arc`, so a clone is cheap
/// and every session shares one warm library registry.
#[derive(Clone)]
pub struct BookrackServer {
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    info_context: LibraryInfoContext,
    tool_router: ToolRouter<BookrackServer>,
}

#[tool_router(router = tool_router)]
impl BookrackServer {
    /// Build a handler over the given library registry. `info_context`
    /// carries the static facts (data dir, library name, ollama url,
    /// embed model) needed to fill `library.info` without re-reading
    /// the process environment on every call. Tools route each call
    /// through the registry, scoped to either an explicit `library`
    /// selector or the current default when the selector is absent.
    pub fn new(
        registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
        info_context: LibraryInfoContext,
    ) -> BookrackServer {
        BookrackServer {
            registry,
            info_context,
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve a [`LibraryHandle`] for the given selector. Read tools
    /// pass `args.library.as_deref()`; write tools demand a name up
    /// front (their args carry `library: String`, not `Option`) so the
    /// resolution never falls back silently.
    ///
    /// An unknown name produces an MCP `InvalidParams` error carrying
    /// the registry's listed available names — the same diagnostic
    /// shape the CLI surface uses for `--library` typos.
    fn resolve_handle(
        &self,
        library: Option<&str>,
    ) -> Result<Arc<LibraryHandle<OllamaEmbedClient>>, ErrorData> {
        self.registry
            .get(library)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))
    }

    /// Search the library and return cited passages as a JSON array.
    #[tool(
        name = "library.search",
        description = "Search the local book library for passages relevant to a \
                       natural-language query. Returns cited passages, nearest \
                       first, each with a breadcrumb trail and source location."
    )]
    async fn library_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let overrides = args.overrides();
        let hits = reads::search::search(handle.ops(), &args.query, overrides, args.top_k)
            .await
            .map_err(ops_error_to_internal)?;
        tracing::info!(hits = hits.len(), "mcp library.search");
        respond_with(&hits)
    }

    /// Search within one book only — the same ranking as `library.search`
    /// but recall is confined to the chunks owned by `intake_id`.
    #[tool(
        name = "library.search_in_book",
        description = "Search a single book for passages relevant to a query. Pass \
                       the book's intake_id (from library.list_books / \
                       library.show_book). Returns cited passages, nearest first."
    )]
    async fn library_search_in_book(
        &self,
        Parameters(args): Parameters<SearchInBookArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let overrides = args.overrides();
        let result = reads::search::search_in_book(
            handle.ops(),
            args.intake_id,
            &args.query,
            overrides,
            args.top_k,
        )
        .await;
        match result {
            Ok(hits) => {
                tracing::info!(
                    intake_id = args.intake_id,
                    hits = hits.len(),
                    "mcp library.search_in_book"
                );
                respond_with(&hits)
            }
            // Preserve the prior wire shape: an unknown intake reads
            // as an empty hit list on this tool, not a fault.
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Vec<bookrack_ops::Citation>>(&Vec::new())
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Aggregate counts over the library (intakes, book states, retrieval
    /// issues).
    #[tool(
        name = "library.stats",
        description = "Return aggregate counts over the library: intakes by status \
                       and format, book states by pipeline stage, and retrieval \
                       issues by triage status."
    )]
    async fn library_stats(
        &self,
        Parameters(args): Parameters<LibraryOnlyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let stats = reads::books::show_stats(handle.ops()).map_err(ops_error_to_internal)?;
        respond_with(&stats)
    }

    /// List books in the library, paginated.
    #[tool(
        name = "library.list_books",
        description = "List books known to the library, paginated. Returns a slice \
                       of book summaries plus the total matching count and a \
                       truncated flag."
    )]
    async fn library_list_books(
        &self,
        Parameters(args): Parameters<ListBooksArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page =
            reads::books::list_books(handle.ops(), limit, offset).map_err(ops_error_to_internal)?;
        tracing::info!(
            returned = page.books.len(),
            total = page.total,
            "mcp library.list_books"
        );
        respond_with(&page)
    }

    /// Find books by title substring, contributor, format, or status.
    #[tool(
        name = "library.find_books",
        description = "Search the book registry by title substring (fuzzy) and / or \
                       contributor name (exact). `categories` is reserved for a \
                       future release and is ignored today."
    )]
    async fn library_find_books(
        &self,
        Parameters(args): Parameters<FindBooksArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        if let Some(cats) = &args.categories
            && !cats.is_empty()
        {
            tracing::warn!(
                categories = ?cats,
                "library.find_books: categories filter is not yet implemented and was ignored"
            );
        }
        let filter = BookFilter {
            title_substring: args.title_substring,
            contributor_name: args.contributor_name,
            contributor_role: args.contributor_role,
            format: args.format,
            ..BookFilter::default()
        };
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page = reads::books::find_books(handle.ops(), filter, limit, offset)
            .map_err(ops_error_to_internal)?;
        tracing::info!(
            returned = page.books.len(),
            total = page.total,
            "mcp library.find_books"
        );
        respond_with(&page)
    }

    /// Fetch the full bibliographic record for one book.
    #[tool(
        name = "library.show_book",
        description = "Fetch the full bibliographic record for one book by intake id. \
                       Returns effective biblio attributes and the contributor list, \
                       or null when no such book is registered."
    )]
    async fn library_show_book(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::books::show_book(handle.ops(), args.intake_id) {
            Ok(detail) => respond_with(&Some(detail)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<bookrack_ops::dto::BookDetail>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return one book's table of contents.
    #[tool(
        name = "library.show_toc",
        description = "Return the table of contents of one book: a depth-first list \
                       of organizing nodes (chapters, sections, ...) with their \
                       titles, depths, and document-order spans. Returns null when \
                       no such book is ingested."
    )]
    async fn library_show_toc(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::books::show_toc(handle.ops(), args.intake_id) {
            Ok(toc) => respond_with(&Some(toc)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<bookrack_ops::dto::Toc>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return the metadata-status read for one book.
    #[tool(
        name = "library.show_metadata_audit",
        description = "Return the metadata-status record for one book: the \
                       bibliographic detail plus the persisted audit verdict, \
                       confidence, and current review status. Returns null when \
                       no such book is registered."
    )]
    async fn library_show_metadata_audit(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::metadata::show_metadata_audit(handle.ops(), args.intake_id) {
            Ok(report) => respond_with(&Some(report)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<bookrack_ops::dto::metadata_report::MetadataReport>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return every registered book with its confidence and review
    /// status, unfiltered.
    #[tool(
        name = "library.list_metadata",
        description = "List every registered book with its current confidence and review \
                       status, regardless of audit verdict. Paginated."
    )]
    async fn library_list_metadata(
        &self,
        Parameters(args): Parameters<MetadataPageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page = reads::metadata::list_metadata(handle.ops(), limit, offset)
            .map_err(ops_error_to_internal)?;
        respond_with(&page)
    }

    /// Return books still on the metadata review queue.
    #[tool(
        name = "library.list_pending_reviews",
        description = "List books whose metadata audit confidence is low or medium \
                       and whose review is still pending or acknowledged. Paginated."
    )]
    async fn library_list_pending_reviews(
        &self,
        Parameters(args): Parameters<MetadataPageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let limit = args.limit.unwrap_or(0);
        let offset = args.offset.unwrap_or(0);
        let page = reads::metadata::list_pending_reviews(handle.ops(), limit, offset)
            .map_err(ops_error_to_internal)?;
        respond_with(&page)
    }

    /// Return the metadata-edit audit trail for one book.
    #[tool(
        name = "library.show_audit_trail",
        description = "Return the metadata-edit audit trail for one book, oldest \
                       first. Returns null when no such book is registered."
    )]
    async fn library_show_audit_trail(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::metadata::show_audit_trail(handle.ops(), args.intake_id) {
            Ok(trail) => respond_with(&Some(trail)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<Vec<bookrack_ops::dto::audit::AuditTrailEntry>>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return the book-level pipeline audit trail for one book.
    #[tool(
        name = "library.show_pipeline_trail",
        description = "Return the book-level pipeline audit trail for one book, \
                       oldest first. Each row records a pipeline sub-step (stage, \
                       outcome, duration, run id). Returns null when no such book \
                       is registered."
    )]
    async fn library_show_pipeline_trail(
        &self,
        Parameters(args): Parameters<BookIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        match reads::pipeline::show_pipeline_trail(handle.ops(), args.intake_id) {
            Ok(trail) => respond_with(&Some(trail)),
            Err(OpsError::IntakeNotFound { .. }) => {
                respond_with::<Option<Vec<bookrack_ops::dto::audit::PipelineAuditEntry>>>(&None)
            }
            Err(e) => Err(ops_error_to_internal(e)),
        }
    }

    /// Return the one-page library status card.
    #[tool(
        name = "library.info",
        description = "Return a one-page status card for the open library: schema \
                       versions, embedder configuration, stamped index parameters, \
                       live row count, intake counts, and approximate disk usage."
    )]
    async fn library_info(
        &self,
        Parameters(args): Parameters<LibraryOnlyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let ctx = self.info_context.clone();
        let info = reads::info::show_library_info(handle.ops(), ctx)
            .await
            .map_err(ops_error_to_internal)?;
        respond_with(&info)
    }

    /// Snapshot the vector store.
    #[tool(
        name = "library.vectors_status",
        description = "Snapshot the vector store: chunk-table row count, every ANN \
                       index LanceDB enumerates with its per-shard statistics, the \
                       persisted ANN config, and any drift between the on-disk meta \
                       and the indices LanceDB actually carries."
    )]
    async fn library_vectors_status(
        &self,
        Parameters(args): Parameters<LibraryOnlyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(args.library.as_deref())?;
        let status = reads::vectors::status(handle.ops())
            .await
            .map_err(ops_error_to_internal)?;
        respond_with(&status)
    }

    /// Set an override on one bibliographic field of a book.
    #[tool(
        name = "library.metadata.set",
        description = "Set an override on one bibliographic field (`title`, \
                       `publisher`, `year`, `language`, ...) of one book. The \
                       extracted value is preserved; the override wins on read. \
                       Appends one audit row tagged `actor_kind=llm`."
    )]
    async fn library_metadata_set(
        &self,
        Parameters(args): Parameters<MetadataSetArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::SetMetadataFieldRequest {
            intake_id: args.intake_id,
            field: args.field,
            value: args.value,
        };
        let outcome = writes::metadata::set_metadata_field(handle.ops(), req)
            .map_err(ops_error_to_internal)?;
        respond_with(&outcome)
    }

    /// Remove an override on one bibliographic field, reverting to the
    /// extracted value.
    #[tool(
        name = "library.metadata.clear",
        description = "Remove an override on one bibliographic field of one book, \
                       reverting to the extracted value. Appends one audit row \
                       even when there was no override to clear, so the trail \
                       records that the operation was attempted."
    )]
    async fn library_metadata_clear(
        &self,
        Parameters(args): Parameters<MetadataClearArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::ClearMetadataFieldRequest {
            intake_id: args.intake_id,
            field: args.field,
        };
        let outcome = writes::metadata::clear_metadata_field(handle.ops(), req)
            .map_err(ops_error_to_internal)?;
        respond_with(&outcome)
    }

    /// Acknowledge a metadata gap: flip the review row to `acknowledged`
    /// with a recorded reason.
    #[tool(
        name = "library.metadata.ack",
        description = "Acknowledge a metadata gap on one book: the audit verdict \
                       is unchanged; the review row is flipped to `acknowledged` \
                       with the given reason. The book stays on the review queue \
                       until approved or rejected."
    )]
    async fn library_metadata_ack(
        &self,
        Parameters(args): Parameters<MetadataAckArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::AcknowledgeMetadataGapRequest {
            intake_id: args.intake_id,
            reason: args.reason,
        };
        let outcome = writes::metadata::acknowledge_metadata_gap(handle.ops(), req)
            .map_err(ops_error_to_internal)?;
        respond_with(&outcome)
    }

    /// Approve the metadata record: flip the review row to `approved`.
    #[tool(
        name = "library.metadata.approve",
        description = "Approve the metadata record on one book: assert that the \
                       effective metadata matches the source. Flips the review \
                       row to `approved`. The audit verdict is unchanged."
    )]
    async fn library_metadata_approve(
        &self,
        Parameters(args): Parameters<MetadataApproveArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::ApproveMetadataRequest {
            intake_id: args.intake_id,
            reason: args.reason,
        };
        let outcome =
            writes::metadata::approve_metadata(handle.ops(), req).map_err(ops_error_to_internal)?;
        respond_with(&outcome)
    }

    /// Reject the book: flip the review row to `rejected`.
    #[tool(
        name = "library.metadata.reject",
        description = "Reject one book: flip the review row to `rejected`. \
                       Pipeline rows stay in place so downstream consumers can \
                       filter on `rejected`. Records the reason on the audit row."
    )]
    async fn library_metadata_reject(
        &self,
        Parameters(args): Parameters<MetadataRejectArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let handle = self.resolve_handle(Some(args.library.as_str()))?;
        let req = bookrack_ops::dto::writes::RejectMetadataRequest {
            intake_id: args.intake_id,
            reason: args.reason,
        };
        let outcome =
            writes::metadata::reject_metadata(handle.ops(), req).map_err(ops_error_to_internal)?;
        respond_with(&outcome)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BookrackServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Search and browse a local, offline library of books. Tools: \
             `library.stats` (counts), `library.list_books` / `library.find_books` \
             (browse and search the registry), `library.show_book` / `library.show_toc` \
             (per-book metadata and table of contents), `library.search` (vector \
             search across the whole library), `library.search_in_book` (vector \
             search confined to one book)."
                .to_string(),
        )
    }

    /// Override the `#[tool_handler]`-generated dispatch so every tool
    /// call records its `source` as `mcp`, regardless of the [`Caller`]
    /// baked into the shared [`Ops`] handle. The `bookrack run` daemon
    /// builds one [`Ops`] tagged `Caller::cli()` and shares it across
    /// REPL, HTTP, and queue worker; without this wrap, every recorded
    /// tool call would read as `source=cli` and the cli / mcp split on
    /// `mcp_tool_calls` would collapse.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        with_source_override(bookrack_ops::ACTOR_DETAIL_MCP, self.tool_router.call(tcc)).await
    }
}

/// Encode `value` to a JSON string and wrap it as the body of a successful
/// tool response. Centralises serialization so every tool returns the same
/// `text` content shape.
fn respond_with<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json =
        serde_json::to_string(value).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Map a generic [`OpsError`] to an MCP internal error.
fn ops_error_to_internal(e: OpsError) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// Bind the streamable-HTTP server at `addr` and serve until the
/// shutdown channel fires.
///
/// The MCP service is mounted at `/mcp`; connect a client as an HTTP MCP
/// server pointed at `http://<addr>/mcp`.
///
/// Every tool resolves its target library through the registry — by
/// the `library` selector when present, by the registry's current
/// default when absent. Write tools require an explicit `library`
/// name in their input; read tools accept `library` as optional.
///
/// `shutdown_rx` lets the caller drive graceful shutdown from a
/// broadcast channel shared with sibling tasks (signal listener, REPL,
/// queue worker). A standalone caller can wire its own `ctrl_c` task
/// into the channel; the embedded daemon-REPL feeds its session-wide
/// shutdown signal through here.
pub async fn serve(
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    info_context: LibraryInfoContext,
    addr: &str,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let service = StreamableHttpService::new(
        move || Ok(BookrackServer::new(registry.clone(), info_context.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind MCP server to {addr}"))?;
    tracing::info!(%addr, "bookrack MCP server listening on /mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .context("serve MCP server")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bookrack_ops::Citation;
    use bookrack_ops::dto::{
        BookDetail, BookSummary, ContributorEntry, LibraryStats, ListBooksResult, Toc, TocNode,
    };
    use bookrack_query::NodeId;

    fn citation(node: i64) -> Citation {
        Citation {
            text: "passage".to_string(),
            breadcrumb: "A Test Book \u{203a} Chapter One".to_string(),
            start_node_id: NodeId::new(node),
            start_char_offset: 0,
            end_node_id: NodeId::new(node),
            end_char_offset: 7,
            norm_chunk_sha256: "sha".to_string(),
            distance: 0.1,
        }
    }

    #[test]
    fn search_citations_serialize_as_an_array_with_bare_int_node_ids() {
        let json = serde_json::to_string(&[citation(100_000_001)]).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(value.is_array());
        assert_eq!(value[0]["start_node_id"], serde_json::json!(100_000_001));
        assert_eq!(value[0]["breadcrumb"], "A Test Book \u{203a} Chapter One");
    }

    #[test]
    fn book_summary_serializes_with_owned_strings() {
        let summary = BookSummary {
            intake_id: 1,
            title: Some("A Title".to_string()),
            format: Some("epub".to_string()),
            status: "extracted".to_string(),
            top_contributor: Some("An Author".to_string()),
        };
        let value = serde_json::to_value(&summary).expect("serialize");
        assert_eq!(value["intake_id"], 1);
        assert_eq!(value["title"], "A Title");
        assert_eq!(value["status"], "extracted");
    }

    #[test]
    fn book_detail_carries_an_ordered_effective_biblio_map() {
        let mut biblio = std::collections::BTreeMap::new();
        biblio.insert("title".to_string(), "T".to_string());
        biblio.insert("publisher".to_string(), "P".to_string());
        let detail = BookDetail {
            intake_id: 7,
            title: Some("T".to_string()),
            format: Some("pdf".to_string()),
            status: "embedded".to_string(),
            effective_biblio: biblio,
            contributors: vec![ContributorEntry {
                role: "author".to_string(),
                ordinal: 0,
                name: "An Author".to_string(),
                nationality: None,
                origin: "extracted".to_string(),
            }],
        };
        let value = serde_json::to_value(&detail).expect("serialize");
        assert_eq!(value["effective_biblio"]["title"], "T");
        assert_eq!(value["contributors"][0]["role"], "author");
    }

    #[test]
    fn list_books_result_carries_total_and_truncated_flag() {
        let page = ListBooksResult {
            books: Vec::new(),
            total: 42,
            truncated: true,
        };
        let value = serde_json::to_value(&page).expect("serialize");
        assert_eq!(value["total"], 42);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn empty_library_stats_serialize_as_empty_maps() {
        let stats = LibraryStats::default();
        let value = serde_json::to_value(&stats).expect("serialize");
        assert!(value["intake_counts_by_status"].is_object());
        assert_eq!(
            value["intake_counts_by_status"].as_object().unwrap().len(),
            0
        );
    }

    #[test]
    fn toc_serializes_with_a_truncated_flag() {
        let toc = Toc {
            intake_id: 1,
            nodes: vec![TocNode {
                node_id: 100_000_001,
                parent_id: None,
                title: Some("Root".to_string()),
                depth: 0,
                ordinal: 0,
                toc_lo: Some(1),
                toc_hi: Some(50),
            }],
            truncated: false,
        };
        let value = serde_json::to_value(&toc).expect("serialize");
        assert_eq!(value["intake_id"], 1);
        assert_eq!(value["nodes"][0]["node_id"], 100_000_001);
        assert_eq!(value["truncated"], false);
    }
}
