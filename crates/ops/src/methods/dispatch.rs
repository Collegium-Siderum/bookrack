// SPDX-License-Identifier: Apache-2.0

//! Run a method of [`super::LIBRARY_METHODS`].
//!
//! [`dispatch`] is the body of every library method: it reads the call's
//! params in the shape its surface takes, resolves the library through
//! the registry, runs the read or the write session, and answers with a
//! [`Reply`]. A failure leaves as a [`DispatchError`] already judged by
//! [`crate::error_class`], so a transport maps its kind onto a code of
//! its own and words nothing itself.
//!
//! Wording is not written here: a failure is classified at the point it
//! happens and carries its own three-part problem.
//! `scripts/error-boundary-check.sh` holds this file to that rule.

// Parallel implementation: control-metadata.

use std::sync::Arc;

use bookrack_config::Config;
use bookrack_core::{ItemKind, KindedNodeId, NodeId};
use bookrack_embed::Embedder;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::params::*;
use super::{MethodSpec, RoutedParams, Surface};
use crate::dto::writes::{
    AcknowledgeMetadataGapRequest, AddContributorOutcome, AddContributorRequest,
    ApproveMetadataRequest, ClearMetadataFieldRequest, ReauditMetadataRequest, ReauditOutcome,
    RejectMetadataRequest, RemoveContributorRequest, SetMetadataFieldRequest,
    VoidMetadataFieldRequest, WriteOutcome,
};
use crate::dto::{BookFilter, MetadataFilter, PaperFilter, UnknownFilterValue, parse_statuses};
use crate::error_class::{self, ClassifiedError};
use crate::reads::info::LibraryInfoContext;
use crate::registry::{LibraryHandle, LibraryRegistry, RegistryError};
use crate::write_session::{ChangeSet, Region, SessionRunError, WriteSession};
use crate::{Caller, Citation, OpsError, audit_overlay, reads, writes};

/// What the host contributes to a method that needs more than the
/// library: the process-side half of a library's status card.
pub trait HostContext: Send + Sync {
    /// The context `library.info` reports beside the counts of the
    /// library `cfg` describes: where the library lives, how it was
    /// selected, and the process's own endpoints.
    fn library_info_context(&self, cfg: &Config) -> eyre::Result<LibraryInfoContext>;
}

/// Everything a call runs against.
pub struct DispatchContext<'a, E: Embedder> {
    /// The libraries the call may be routed to.
    pub registry: &'a LibraryRegistry<E>,
    /// Who the call is attributed to: every write session opened for it
    /// records this caller.
    pub caller: &'a Caller,
    /// The host's side of the methods that need it.
    pub host: &'a dyn HostContext,
}

/// A method's answer.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The answer serialized from its typed result, fields in their
    /// declared order: the text an MCP tool returns.
    pub text: String,
    /// The same answer as a JSON value: the control plane's `result`,
    /// and an MCP tool's structured content.
    pub value: Value,
}

/// Why a call failed.
#[derive(Debug)]
pub enum DispatchError {
    /// The failure as [`crate::error_class`] judged it.
    Judged(ClassifiedError),
    /// A [`HostContext`] hook failed. The failure may carry the host's
    /// own error types, so the host judges it.
    Host {
        /// The wire name of the method whose hook failed.
        method: &'static str,
        /// The hook's error.
        report: eyre::Report,
    },
}

/// Run `spec` for a call that arrived on `surface` with `params`.
///
/// `params` absent or `null` reads as an empty object, so a method whose
/// params are all optional takes no params at all.
pub async fn dispatch<E>(
    ctx: &DispatchContext<'_, E>,
    spec: &MethodSpec,
    surface: Surface,
    params: Option<Value>,
) -> Result<Reply, DispatchError>
where
    E: Embedder + Send + Sync + 'static,
{
    let method = spec.wire_name(surface);
    let params = match params {
        None | Some(Value::Null) => json!({}),
        Some(value) => value,
    };
    match spec.name {
        "library.info" => {
            let handle = match surface {
                Surface::ControlPlane => {
                    resolve(ctx, &parse::<LibraryInfoParams>(method, params)?)?
                }
                Surface::Mcp => resolve(ctx, &parse::<LibraryOnlyParams>(method, params)?)?,
            };
            let info_context = ctx
                .host
                .library_info_context(handle.cfg())
                .map_err(|report| DispatchError::Host { method, report })?;
            let info = reads::info::show_library_info(handle.ops(), info_context)
                .await
                .map_err(ops)?;
            reply(&info)
        }
        "library.stats" => {
            let p: LibraryOnlyParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            reply(&reads::books::show_stats(handle.ops()).map_err(ops)?)
        }
        "library.categories" => {
            let p: LibraryOnlyParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            reply(&reads::books::category_counts(handle.ops()).map_err(ops)?)
        }
        "library.list_books" => {
            let p: ListBooksParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let page =
                reads::books::list_books(handle.ops(), page_limit(p.limit), offset(p.offset))
                    .map_err(ops)?;
            reply(&page)
        }
        "library.find_books" => {
            let p: FindBooksParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let statuses = parse_statuses(ItemKind::Book, &p.statuses.unwrap_or_default())
                .map_err(|unknown| filter(&unknown))?;
            let filter = BookFilter {
                title_substring: p.title_substring,
                contributor_name: p.contributor_name,
                contributor_role: p.contributor_role,
                format: p.format,
                statuses,
                categories: p.categories.unwrap_or_default(),
                language: p.language.unwrap_or_default(),
                awaiting_text: p.awaiting_text.unwrap_or(false),
            };
            let page = reads::books::find_books(
                handle.ops(),
                filter,
                page_limit(p.limit),
                offset(p.offset),
            )
            .map_err(ops)?;
            reply(&page)
        }
        "library.show_book" => {
            let p: BookIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::books::show_book(handle.ops(), p.intake_id))
        }
        "library.show_toc" => {
            let p: ShowTocParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::books::show_toc(
                handle.ops(),
                p.intake_id,
                &p.toc_args(),
            ))
        }
        "library.read_context" => {
            let p: ReadContextParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let before = p
                .before
                .unwrap_or(bookrack_query::dto::DEFAULT_CONTEXT_RADIUS);
            let after = p
                .after
                .unwrap_or(bookrack_query::dto::DEFAULT_CONTEXT_RADIUS);
            let target = KindedNodeId {
                kind: p.kind,
                node_id: NodeId::new(p.node_id),
            };
            node_or_null(reads::passages::read_context(
                handle.ops(),
                target,
                before,
                after,
            ))
        }
        "library.read_span" => {
            let p: ReadSpanParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let target = KindedNodeId {
                kind: p.kind,
                node_id: NodeId::new(p.node_id),
            };
            node_or_null(reads::passages::read_span(
                handle.ops(),
                target,
                p.start_after,
            ))
        }
        "library.show_metadata_audit" => {
            let p: BookIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::metadata::show_metadata_audit(
                handle.ops(),
                p.intake_id,
            ))
        }
        "library.show_metadata_report" => {
            let p: BookIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            // The overlays live under the target library's data root, so
            // the report grades under the same rules a re-audit of the
            // book would store.
            let audit_data = audit_overlay::load_audit_data(handle.cfg());
            let audit_profile = audit_overlay::load_audit_profile(handle.cfg(), None);
            or_null(reads::metadata::show_metadata_report(
                handle.ops(),
                p.intake_id,
                &audit_data,
                &audit_profile,
            ))
        }
        "library.list_metadata" => {
            let p: ListMetadataParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let filter = metadata_filter(&p)?;
            let page = reads::metadata::list_metadata(
                handle.ops(),
                filter,
                page_limit(p.limit),
                offset(p.offset),
            )
            .map_err(ops)?;
            reply(&page)
        }
        "library.list_pending_reviews" => {
            let p: PageParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let page = reads::metadata::list_pending_reviews(
                handle.ops(),
                page_limit(p.limit),
                offset(p.offset),
            )
            .map_err(ops)?;
            reply(&page)
        }
        "library.show_audit_trail" => {
            let p: BookIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::metadata::show_audit_trail(handle.ops(), p.intake_id))
        }
        "library.show_pipeline_trail" => {
            let p: BookIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::pipeline::show_pipeline_trail(
                handle.ops(),
                p.intake_id,
            ))
        }
        "library.list_papers" => {
            let p: ListPapersParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let page =
                reads::papers::list_papers(handle.ops(), page_limit(p.limit), offset(p.offset))
                    .map_err(ops)?;
            reply(&page)
        }
        "library.find_papers" => {
            let p: FindPapersParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let statuses = parse_statuses(ItemKind::Paper, &p.statuses.unwrap_or_default())
                .map_err(|unknown| filter(&unknown))?;
            let filter = PaperFilter {
                title_substring: p.title_substring,
                contributor_name: p.contributor_name,
                contributor_role: p.contributor_role,
                statuses,
                year: p.year,
                venue_substring: p.venue_substring,
                doi: p.doi,
                language: p.language.unwrap_or_default(),
            };
            let page = reads::papers::find_papers(
                handle.ops(),
                filter,
                page_limit(p.limit),
                offset(p.offset),
            )
            .map_err(ops)?;
            reply(&page)
        }
        "library.show_paper" => {
            let p: PaperIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::papers::show_paper(handle.ops(), p.intake_id))
        }
        "library.show_paper_toc" => {
            let p: ShowTocParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::papers::show_paper_toc(
                handle.ops(),
                p.intake_id,
                &p.toc_args(),
            ))
        }
        "library.show_paper_metadata_report" => {
            let p: PaperAuditReadParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            audit_overlay::require_known_profile(
                p.audit_profile.as_deref(),
                crate::PAPER_AUDIT_BUILT_IN_NAMES,
            )
            .map_err(ops)?;
            // The same rules `papers.metadata.reaudit` writes with, read
            // from the target library's own data root.
            let audit_profile =
                audit_overlay::load_paper_audit_profile(handle.cfg(), p.audit_profile.as_deref());
            let audit_data = audit_overlay::load_paper_audit_data(handle.cfg());
            or_null(reads::papers_metadata::show_paper_metadata_report(
                handle.ops(),
                p.intake_id,
                &audit_data,
                &audit_profile,
            ))
        }
        "library.show_paper_audit_trail" => {
            let p: PaperIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::papers_metadata::show_paper_audit_trail(
                handle.ops(),
                p.intake_id,
            ))
        }
        "library.list_paper_metadata" => {
            let p: ListMetadataParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let filter = metadata_filter(&p)?;
            let page = reads::papers_metadata::list_paper_metadata(
                handle.ops(),
                filter,
                page_limit(p.limit),
                offset(p.offset),
            )
            .map_err(ops)?;
            reply(&page)
        }
        "library.list_paper_pending_reviews" => {
            let p: PageParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let page = reads::papers_metadata::list_paper_pending_reviews(
                handle.ops(),
                page_limit(p.limit),
                offset(p.offset),
            )
            .map_err(ops)?;
            reply(&page)
        }
        "papers.export_csl" => {
            let p: PaperIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::papers::export_csl(handle.ops(), p.intake_id))
        }
        "papers.fetch_source" => {
            let p: PaperIdParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            or_null(reads::papers::fetch_source(handle.ops(), p.intake_id))
        }
        "library.search" => {
            let p: SearchParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let kind = p.checked_kind().map_err(refused)?;
            let book = p.overrides_excluding(&p.exclude_book_intake_ids);
            let paper = p.overrides_excluding(&p.exclude_paper_intake_ids);
            let hits = match kind {
                "book" => reads::search::search(handle.ops(), &p.query, book, p.top_k).await,
                "paper" => {
                    reads::search::search_paper(handle.ops(), &p.query, paper, p.top_k).await
                }
                "all" => {
                    reads::search::search_unified(handle.ops(), &p.query, book, paper, p.top_k)
                        .await
                }
                other => {
                    return Err(refused(ParamsError::UnknownSearchKind {
                        method,
                        kind: other.to_owned(),
                    }));
                }
            }
            .map_err(ops)?;
            tracing::info!(method, kind, hits = hits.len(), "search answered");
            reply(&hits)
        }
        "library.search_in_book" => {
            let p: SearchInBookParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let result = reads::search::search_in_book(
                handle.ops(),
                p.intake_id,
                &p.query,
                p.overrides(),
                p.top_k,
            )
            .await;
            hits_or_empty(result)
        }
        "library.search_in_paper" => {
            let p: SearchInPaperParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let result = reads::search::search_in_paper(
                handle.ops(),
                p.intake_id,
                &p.query,
                p.overrides(),
                p.top_k,
            )
            .await;
            hits_or_empty(result)
        }
        "library.vectors_status" => {
            let p: LibraryOnlyParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            reply(&reads::vectors::status(handle.ops()).await.map_err(ops)?)
        }
        "reference.lookup" => {
            let p: ReferenceLookupParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let catalogs = reads::reference::catalogs().map_err(reference)?;
            let result = reads::reference::lookup_at(
                &handle.ops().reference_db_path(),
                catalogs,
                &p.to_lookup(),
            )
            .map_err(reference)?;
            reply(&result)
        }
        "reference.overlay_set" => {
            let p: ReferenceOverlaySetParams = parse(method, params)?;
            let handle = resolve(ctx, &p)?;
            let request = p.to_request();
            // Checked before a session opens, so a refused payload
            // neither waits on another write nor creates the store.
            let catalogs = reads::reference::catalogs().map_err(reference)?;
            writes::reference::validate_overlay(catalogs, &request).map_err(reference)?;
            let region = Region::Reference {
                book_slug: request.book_slug.clone(),
            };
            let session = ctx
                .registry
                .begin_write(&handle, region, ctx.caller.clone())
                .map_err(|busy| ops(OpsError::WriteBusy(busy)))?;
            let receipt = session
                .run_blocking(async move |s| writes::reference::set_overlay(s, &request))
                .await
                .map_err(|e| session_failure(e, error_class::from_reference))?;
            reply(&receipt)
        }
        "metadata.set"
        | "metadata.clear"
        | "metadata.void"
        | "metadata.reaudit"
        | "metadata.contributor_add"
        | "metadata.contributor_remove"
        | "metadata.ack"
        | "metadata.approve"
        | "metadata.reject" => {
            let call = book_write(spec.name, method, surface, params)?;
            book_metadata_write(ctx, surface, call).await
        }
        // Every row of the table has an arm above; the table's own test
        // drives each row through here.
        _ => Err(fault(&NoBody { method })),
    }
}

/// A method the table lists that has no body here.
#[derive(Debug, thiserror::Error)]
#[error("{method} has no body in the method table")]
struct NoBody {
    method: &'static str,
}

/// A book-side metadata write as both surfaces ask for it: the library
/// selector, the book, and what to do.
struct BookWriteCall {
    library: Option<String>,
    book: i64,
    action: BookWrite,
}

/// The nine book-side metadata writes.
enum BookWrite {
    Set(SetMetadataFieldRequest),
    Clear(ClearMetadataFieldRequest),
    Void(VoidMetadataFieldRequest),
    Reaudit(ReauditMetadataRequest, Option<String>),
    ContributorAdd(AddContributorRequest),
    ContributorRemove(RemoveContributorRequest),
    Ack(AcknowledgeMetadataGapRequest),
    Approve(ApproveMetadataRequest),
    Reject(RejectMetadataRequest),
}

/// What a book-side metadata write did.
enum BookWriteOutcome {
    /// An edit, and the field it touched when the action names one.
    Edit(Option<String>, WriteOutcome),
    /// A contributor attribution and the row it created.
    ContributorAdded(AddContributorOutcome),
    /// A re-audit and the judgement it stored.
    Reaudited(ReauditOutcome),
}

/// Read a book-side metadata write's params in the shape `surface`
/// takes for the method the control plane calls `name`.
fn book_write(
    name: &str,
    method: &'static str,
    surface: Surface,
    params: Value,
) -> Result<BookWriteCall, DispatchError> {
    let call = |library, book, action| BookWriteCall {
        library,
        book,
        action,
    };
    Ok(match (surface, name) {
        (Surface::ControlPlane, "metadata.set") => {
            let p: MetadataSetParams = parse(method, params)?;
            let request = SetMetadataFieldRequest {
                intake_id: p.book,
                field: p.field,
                value: p.value,
                reason: p.reason,
                confirmed: p.confirmed,
            };
            call(p.library, p.book, BookWrite::Set(request))
        }
        (Surface::Mcp, "metadata.set") => {
            let p: LibraryMetadataSetParams = parse(method, params)?;
            let request = SetMetadataFieldRequest {
                intake_id: p.intake_id,
                field: p.field,
                value: p.value,
                reason: Some(p.reason),
                confirmed: p.confirmed.unwrap_or(false),
            };
            call(Some(p.library), p.intake_id, BookWrite::Set(request))
        }
        (Surface::ControlPlane, "metadata.clear") => {
            let p: MetadataClearParams = parse(method, params)?;
            let request = ClearMetadataFieldRequest {
                intake_id: p.book,
                field: p.field,
                reason: p.reason,
            };
            call(p.library, p.book, BookWrite::Clear(request))
        }
        (Surface::Mcp, "metadata.clear") => {
            let p: LibraryMetadataClearParams = parse(method, params)?;
            let request = ClearMetadataFieldRequest {
                intake_id: p.intake_id,
                field: p.field,
                reason: Some(p.reason),
            };
            call(Some(p.library), p.intake_id, BookWrite::Clear(request))
        }
        (Surface::ControlPlane, "metadata.void") => {
            let p: MetadataVoidParams = parse(method, params)?;
            let request = VoidMetadataFieldRequest {
                intake_id: p.book,
                field: p.field,
                reason: p.reason,
            };
            call(p.library, p.book, BookWrite::Void(request))
        }
        (Surface::Mcp, "metadata.void") => {
            let p: LibraryMetadataVoidParams = parse(method, params)?;
            let request = VoidMetadataFieldRequest {
                intake_id: p.intake_id,
                field: p.field,
                reason: Some(p.reason),
            };
            call(Some(p.library), p.intake_id, BookWrite::Void(request))
        }
        (Surface::ControlPlane, "metadata.reaudit") => {
            let p: MetadataReauditParams = parse(method, params)?;
            let request = ReauditMetadataRequest { intake_id: p.book };
            call(
                p.library,
                p.book,
                BookWrite::Reaudit(request, p.audit_profile),
            )
        }
        (Surface::Mcp, "metadata.reaudit") => {
            let p: LibraryMetadataReauditParams = parse(method, params)?;
            let request = ReauditMetadataRequest {
                intake_id: p.intake_id,
            };
            call(
                Some(p.library),
                p.intake_id,
                BookWrite::Reaudit(request, None),
            )
        }
        (Surface::ControlPlane, "metadata.contributor_add") => {
            let p: MetadataContributorAddParams = parse(method, params)?;
            let request = AddContributorRequest {
                intake_id: p.book,
                role: p.role,
                name: p.name,
                nationality: p.nationality,
                reason: p.reason,
            };
            call(p.library, p.book, BookWrite::ContributorAdd(request))
        }
        (Surface::Mcp, "metadata.contributor_add") => {
            let p: LibraryMetadataContributorAddParams = parse(method, params)?;
            let request = AddContributorRequest {
                intake_id: p.intake_id,
                role: p.role,
                name: p.name,
                nationality: p.nationality,
                reason: Some(p.reason),
            };
            call(
                Some(p.library),
                p.intake_id,
                BookWrite::ContributorAdd(request),
            )
        }
        (Surface::ControlPlane, "metadata.contributor_remove") => {
            let p: MetadataContributorRemoveParams = parse(method, params)?;
            let request = RemoveContributorRequest {
                intake_id: p.book,
                contributor_id: p.contributor_id,
                reason: p.reason,
            };
            call(p.library, p.book, BookWrite::ContributorRemove(request))
        }
        (Surface::Mcp, "metadata.contributor_remove") => {
            let p: LibraryMetadataContributorRemoveParams = parse(method, params)?;
            let request = RemoveContributorRequest {
                intake_id: p.intake_id,
                contributor_id: p.contributor_id,
                reason: Some(p.reason),
            };
            call(
                Some(p.library),
                p.intake_id,
                BookWrite::ContributorRemove(request),
            )
        }
        (Surface::ControlPlane, "metadata.ack") => {
            let p: MetadataAckParams = parse(method, params)?;
            let request = AcknowledgeMetadataGapRequest {
                intake_id: p.book,
                reason: p.reason,
            };
            call(p.library, p.book, BookWrite::Ack(request))
        }
        (Surface::Mcp, "metadata.ack") => {
            let p: LibraryMetadataAckParams = parse(method, params)?;
            let request = AcknowledgeMetadataGapRequest {
                intake_id: p.intake_id,
                reason: p.reason,
            };
            call(Some(p.library), p.intake_id, BookWrite::Ack(request))
        }
        (Surface::ControlPlane, "metadata.approve") => {
            let p: MetadataApproveParams = parse(method, params)?;
            let request = ApproveMetadataRequest {
                intake_id: p.book,
                reason: p.reason,
            };
            call(p.library, p.book, BookWrite::Approve(request))
        }
        (Surface::Mcp, "metadata.approve") => {
            let p: LibraryMetadataApproveParams = parse(method, params)?;
            let request = ApproveMetadataRequest {
                intake_id: p.intake_id,
                reason: p.reason,
            };
            call(Some(p.library), p.intake_id, BookWrite::Approve(request))
        }
        (Surface::ControlPlane, "metadata.reject") => {
            let p: MetadataRejectParams = parse(method, params)?;
            let request = RejectMetadataRequest {
                intake_id: p.book,
                reason: p.reason,
            };
            call(p.library, p.book, BookWrite::Reject(request))
        }
        (Surface::Mcp, "metadata.reject") => {
            let p: LibraryMetadataRejectParams = parse(method, params)?;
            let request = RejectMetadataRequest {
                intake_id: p.intake_id,
                reason: p.reason,
            };
            call(Some(p.library), p.intake_id, BookWrite::Reject(request))
        }
        _ => return Err(fault(&NoBody { method })),
    })
}

/// Run a book-side metadata write in a curation session over the book,
/// attributed to the context's caller, and answer in `surface`'s shape.
///
/// The session takes the daemon's write mutex without waiting, so a
/// concurrent write is answered busy; the write runs on a blocking task,
/// and the library is announced once it returns. A re-audit reads the
/// overlays inside the session, from the library it writes, so an
/// edited overlay applies to the next call without a remount.
async fn book_metadata_write<E>(
    ctx: &DispatchContext<'_, E>,
    surface: Surface,
    call: BookWriteCall,
) -> Result<Reply, DispatchError>
where
    E: Embedder + Send + Sync + 'static,
{
    let handle = ctx
        .registry
        .get(call.library.as_deref())
        .map_err(|e| registry(&e))?;
    if let BookWrite::Reaudit(_, Some(profile)) = &call.action {
        audit_overlay::require_known_profile(
            Some(profile.as_str()),
            bookrack_audit_profile::ALL_BUILT_IN_NAMES,
        )
        .map_err(ops)?;
    }
    let region = Region::Curation {
        kind: ItemKind::Book,
        intake_id: call.book,
    };
    let session = ctx
        .registry
        .begin_write(&handle, region, ctx.caller.clone())
        .map_err(|busy| ops(OpsError::WriteBusy(busy)))?;
    let library = String::from(handle.name());
    let announced = library.clone();
    let action = call.action;
    let outcome = session
        .run_blocking(async move |s| {
            let outcome = run_book_write(s, action)?;
            Ok::<_, OpsError>((outcome, ChangeSet::announced(announced)))
        })
        .await
        .map_err(|e| session_failure(e, error_class::from_ops))?;
    match surface {
        Surface::Mcp => match &outcome {
            BookWriteOutcome::Edit(_, outcome) => reply(outcome),
            BookWriteOutcome::ContributorAdded(outcome) => reply(outcome),
            BookWriteOutcome::Reaudited(outcome) => reply(outcome),
        },
        Surface::ControlPlane => control_write_reply(&library, call.book, outcome),
    }
}

/// Run one book-side metadata write in `session`.
fn run_book_write<E: Embedder>(
    session: &mut WriteSession<E>,
    action: BookWrite,
) -> crate::Result<BookWriteOutcome> {
    use crate::writes::metadata as w;
    Ok(match action {
        BookWrite::Set(request) => {
            let field = Some(request.field.clone());
            BookWriteOutcome::Edit(field, w::set_metadata_field(session, request)?)
        }
        BookWrite::Clear(request) => {
            let field = Some(request.field.clone());
            BookWriteOutcome::Edit(field, w::clear_metadata_field(session, request)?)
        }
        BookWrite::Void(request) => {
            let field = Some(request.field.clone());
            BookWriteOutcome::Edit(field, w::void_metadata_field(session, request)?)
        }
        BookWrite::Reaudit(request, profile) => {
            let cfg = session.handle().cfg();
            let audit_data = audit_overlay::load_audit_data(cfg);
            let audit_profile = audit_overlay::load_audit_profile(cfg, profile.as_deref());
            BookWriteOutcome::Reaudited(w::reaudit_metadata(
                session,
                request,
                &audit_data,
                &audit_profile,
            )?)
        }
        BookWrite::ContributorAdd(request) => {
            BookWriteOutcome::ContributorAdded(w::add_contributor(session, request)?)
        }
        BookWrite::ContributorRemove(request) => {
            BookWriteOutcome::Edit(None, w::remove_contributor(session, request)?)
        }
        BookWrite::Ack(request) => {
            BookWriteOutcome::Edit(None, w::acknowledge_metadata_gap(session, request)?)
        }
        BookWrite::Approve(request) => {
            BookWriteOutcome::Edit(None, w::approve_metadata(session, request)?)
        }
        BookWrite::Reject(request) => {
            BookWriteOutcome::Edit(None, w::reject_metadata(session, request)?)
        }
    })
}

/// The control plane's reply to a book-side metadata write: what the
/// write did, under the book it addressed, with `ok` and the library
/// beside it.
fn control_write_reply(
    library: &str,
    book: i64,
    outcome: BookWriteOutcome,
) -> Result<Reply, DispatchError> {
    #[derive(Serialize)]
    struct Edit<'a> {
        ok: bool,
        library: &'a str,
        book: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        field: Option<String>,
        #[serde(flatten)]
        outcome: WriteOutcome,
    }
    #[derive(Serialize)]
    struct ContributorAdded<'a> {
        ok: bool,
        library: &'a str,
        book: i64,
        #[serde(flatten)]
        outcome: AddContributorOutcome,
    }
    #[derive(Serialize)]
    struct Reaudited<'a> {
        ok: bool,
        library: &'a str,
        book: i64,
        verdict: String,
        previous_verdict: Option<String>,
        confidence: String,
        previous_confidence: Option<String>,
    }
    match outcome {
        BookWriteOutcome::Edit(field, outcome) => reply(&Edit {
            ok: true,
            library,
            book,
            field,
            outcome,
        }),
        BookWriteOutcome::ContributorAdded(outcome) => reply(&ContributorAdded {
            ok: true,
            library,
            book,
            outcome,
        }),
        BookWriteOutcome::Reaudited(outcome) => reply(&Reaudited {
            ok: true,
            library,
            book,
            verdict: outcome.verdict,
            previous_verdict: outcome.previous_verdict,
            confidence: outcome.confidence,
            previous_confidence: outcome.previous_confidence,
        }),
    }
}

/// Deserialize a call's params into `T`, refusing them as the method
/// `method` names when they do not fit.
fn parse<T: DeserializeOwned>(method: &'static str, params: Value) -> Result<T, DispatchError> {
    serde_json::from_value(params).map_err(|e| {
        refused(ParamsError::Malformed {
            method,
            message: format!("{e}"),
        })
    })
}

/// The handle the params' selection routes to.
fn resolve<E: Embedder, P: RoutedParams>(
    ctx: &DispatchContext<'_, E>,
    params: &P,
) -> Result<Arc<LibraryHandle<E>>, DispatchError> {
    ctx.registry.get(params.library()).map_err(|e| registry(&e))
}

/// Serialize `value` into both forms of a [`Reply`].
fn reply<T: Serialize + ?Sized>(value: &T) -> Result<Reply, DispatchError> {
    let text = serde_json::to_string(value).map_err(|e| fault(&e))?;
    let value = serde_json::to_value(value).map_err(|e| fault(&e))?;
    Ok(Reply { text, value })
}

/// An item read's answer: the item, or `null` when no intake has the
/// id.
fn or_null<T: Serialize>(result: crate::Result<T>) -> Result<Reply, DispatchError> {
    match result {
        Ok(item) => reply(&Some(item)),
        Err(OpsError::IntakeNotFound { .. }) => reply(&None::<T>),
        Err(e) => Err(ops(e)),
    }
}

/// A passage read's answer: the passages, or `null` when no node has
/// the id.
fn node_or_null<T: Serialize>(result: crate::Result<T>) -> Result<Reply, DispatchError> {
    match result {
        Ok(read) => reply(&Some(read)),
        Err(OpsError::NodeNotFound { .. }) => reply(&None::<T>),
        Err(e) => Err(ops(e)),
    }
}

/// A confined search's answer: the hits, or none when no intake has
/// the id.
fn hits_or_empty(result: crate::Result<Vec<Citation>>) -> Result<Reply, DispatchError> {
    match result {
        Ok(hits) => reply(&hits),
        Err(OpsError::IntakeNotFound { .. }) => reply(&Vec::<Citation>::new()),
        Err(e) => Err(ops(e)),
    }
}

/// The metadata-list filter the params name, with each vocabulary
/// checked.
fn metadata_filter(p: &ListMetadataParams) -> Result<MetadataFilter, DispatchError> {
    MetadataFilter::checked(
        p.title_substring.clone(),
        p.confidence_in.clone().unwrap_or_default(),
        p.review_status_in.clone().unwrap_or_default(),
    )
    .map_err(|unknown| filter(&unknown))
}

/// A page size as the reads take it: absent is `0`, the reads' own
/// default.
fn page_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(0)
}

/// A page offset as the reads take it.
fn offset(offset: Option<u32>) -> u32 {
    offset.unwrap_or(0)
}

/// A session that did not settle as its write: the write's own error,
/// judged by `judge`, or a fault of the session around it.
fn session_failure<Err>(
    e: SessionRunError<Err>,
    judge: impl FnOnce(&Err) -> ClassifiedError,
) -> DispatchError
where
    Err: std::error::Error + Send + Sync + 'static,
{
    match e {
        SessionRunError::Op(e) => DispatchError::Judged(judge(&e)),
        other => fault(&other),
    }
}

fn ops(e: OpsError) -> DispatchError {
    DispatchError::Judged(error_class::from_ops(&e))
}

fn registry(e: &RegistryError) -> DispatchError {
    DispatchError::Judged(error_class::from_registry(e))
}

fn reference(e: crate::reads::reference::ReferenceError) -> DispatchError {
    DispatchError::Judged(error_class::from_reference(&e))
}

fn filter(unknown: &UnknownFilterValue) -> DispatchError {
    DispatchError::Judged(error_class::from_unknown_filter(unknown))
}

fn refused(e: ParamsError) -> DispatchError {
    DispatchError::Judged(error_class::from_params(&e))
}

fn fault(e: &(dyn std::error::Error + 'static)) -> DispatchError {
    DispatchError::Judged(error_class::from_server_fault(e))
}
