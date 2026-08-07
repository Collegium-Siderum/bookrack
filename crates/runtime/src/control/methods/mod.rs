// SPDX-License-Identifier: Apache-2.0

//! Control-plane method table.
//!
//! [`dispatch`] is the only entry point: hand it a parsed
//! [`Request`] and a [`MethodContext`], get back either the JSON
//! payload that becomes the response's `result` or an [`RpcError`].
//!
//! Phase 1 carries `daemon.*`, `status`, `doctor.gather`,
//! `queue.list`, `library.*`, and `events.snapshot`; Phase 2 layers
//! `ingest.*`, `metadata.*`, `vectors.*`, `corpus.rebuild`,
//! `stamps.reconcile`, `remove`, and `dryrun` on top, each wrapped
//! through [`run_write`] so the write mutex, daemon-state transitions,
//! and broadcast notifications fire in the same order for every
//! handler.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use bookrack_config::Config;
use bookrack_config::LibrarySelection;
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::stream::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::LibraryRegistry;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, Notify, OwnedMutexGuard, broadcast};

use super::error_map::rpc_from_problem;
use super::events::{Event, EventStreamHandle};
use super::jsonrpc::{
    BUSY, CONFIRMATION_REQUIRED, INVALID_PARAMS, METHOD_NOT_FOUND, Request, RpcError,
};
use super::plan_registry::PlanRegistry;
use crate::cmd::input_error::CmdInputError;

/// A params type that carries a library selection.
///
/// The implementation is what makes a `routed` row in [`methods!`]
/// checkable: the row names the params type, the impl names the key
/// and reads the field behind it, so a method filed as routed whose
/// params have no such field fails to compile. Without it the axis
/// would be a hand-kept assertion about code somewhere else, and the
/// failure mode of a wrong row is silent — a selection the caller
/// spelled out reaches a handler that never looks for it.
pub trait RoutedParams {
    /// The params key that names the library. `"library"` for every
    /// method but `library.info`, whose own `name` parameter predates
    /// the shared spelling.
    const LIBRARY_KEY: &'static str;

    /// The selection this call carries, `None` when the caller left
    /// the key out and the registry's default applies.
    fn library(&self) -> Option<&str>;
}

/// Implement [`RoutedParams`] for params types that spell the key
/// `library`. Invoked in the module that owns the type, so the field
/// need not be public.
macro_rules! routed_params {
    ($( $t:ty ),+ $(,)?) => {
        $(
            impl $crate::control::methods::RoutedParams for $t {
                const LIBRARY_KEY: &'static str = "library";

                fn library(&self) -> Option<&str> {
                    self.library.as_deref()
                }
            }
        )+
    };
}

pub mod corpus;
pub mod diagnose;
pub mod dryrun;
pub mod glean;
pub mod ingest;
pub mod intake;
pub mod libraries;
pub mod logs;
pub mod meta;
pub mod metadata;
pub mod papers_corpus;
pub mod papers_dryrun;
pub mod papers_metadata;
pub mod papers_remove;
pub mod papers_stamps;
pub mod papers_vectors;
pub mod queue_writes;
pub mod reads;
pub mod reads_library;
pub mod remove;
pub mod stamps;
pub mod tray;
pub mod vectors;
pub mod verify;

pub use reads::SNAPSHOT_CHANNELS;
pub use reads::snapshot_for;

/// Render a [`CmdInputError`] straight onto the wire envelope.
///
/// A handler that owns its own error mapping never hands an
/// `eyre::Report` to `write_err`, so the cause-chain downcast there
/// does not reach it. What the shared type still buys such a handler
/// is the wording and the code the book side already produces for the
/// same refusal — which is what keeps the two sides from drifting
/// apart one `format!` at a time.
///
/// [`CmdInputError::TargetDrifted`] carries its own code and does not
/// belong on this path; no handler that maps its own errors mints a
/// plan.
pub(super) fn input_err(e: CmdInputError) -> RpcError {
    use bookrack_core::Explain;
    rpc_from_problem(INVALID_PARAMS, e.explain())
}

/// Read-mostly handles the dispatcher reaches into. The runtime owns
/// the originals; the dispatcher only clones cheap shared handles.
#[derive(Clone)]
pub struct MethodContext {
    pub registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    pub info_context: LibraryInfoContext,
    pub queue_state: Arc<Mutex<QueueState>>,
    pub queue_state_path: PathBuf,
    pub event_stream: EventStreamHandle,
    pub write_guard: Arc<TokioMutex<()>>,
    pub shutdown_tx: broadcast::Sender<()>,
    pub started_at_rfc3339: String,
    pub selection: LibrarySelection,
    /// Name of the primary (bring-up-selected) library. A single-value
    /// snapshot consumed by the queue worker's no-name fallback, the
    /// `status` card, and the `library.changed` snapshot a subscriber
    /// receives on connect; per-library status surfaces are a later
    /// milestone. A `library.changed` published *by a write* names the
    /// library that write touched instead — [`run_write`] takes it
    /// from the handler's own handle.
    pub library_name: String,
    /// Cached MCP tool list, populated by the daemon at startup from
    /// `bookrack_mcp::list_tools()`. Empty in entry points that do
    /// not bring up the MCP listener.
    pub mcp_tools: Arc<Vec<meta::McpToolInfo>>,
    /// `true` when the runtime spawned a queue worker. Headless
    /// `bookrack-mcp` entries leave it `false`, in which case the
    /// dispatch routes queue-bound write methods to a
    /// `-32002 not_ready` response without invoking the handler.
    pub queue_worker_enabled: bool,
    /// Notification handle the GUI tray (if any) waits on. The
    /// `tray.focus` method signals one waiter per call; with no GUI
    /// attached the notification has no consumer and the call is a
    /// no-op.
    pub tray_focus_signal: Arc<Notify>,
    /// The supervised llama-server, when the effective profile enables
    /// a reranker and no operator URL overrides the backend. `doctor.
    /// gather` reads its state for the backend row.
    pub rerank_supervisor: Option<Arc<crate::rerank_supervisor::RerankSupervisor>>,
    /// The control socket this daemon is answering on. `doctor.gather`
    /// names it so a report says which daemon produced it. `None` in
    /// entry points that dispatch without a bound socket.
    pub control_socket: Option<PathBuf>,
    /// Worker-loop pause flag. The `queue.pause` / `queue.resume`
    /// handlers flip this atomic; the worker loop reads it before
    /// pulling the next pending job. Mirrored onto
    /// `QueueState::paused` so the on-disk snapshot agrees with the
    /// in-memory behaviour.
    pub queue_paused: Arc<AtomicBool>,
    /// In-process log fan-out handle, shared with MCP. Backs
    /// `logs.tail` (and the `log` event channel via the bridge in the
    /// daemon bring-up).
    pub log_stream: LogStreamHandle,
    /// Server-held registry of pinned plans for two-phase destructive
    /// RPCs. Constructed once at daemon bring-up; see
    /// [`super::plan_registry`] for the semantics.
    pub plan_registry: Arc<PlanRegistry>,
    /// The capability to change the mounted set, carrying what a mount
    /// needs and this context does not: the reranker stage a new handle
    /// clones, the caller attribution its writes take, and the ability
    /// to lock a data root. `None` in an entry point that dispatches
    /// without a daemon bring-up behind it, where `library.mount` and
    /// `library.unmount` answer [`NOT_READY`] — the same shape a
    /// queue-bound method takes when no worker was spawned.
    pub mounter: Option<Arc<crate::mount::Mounter>>,
}

/// One of two terminal outcomes a method handler can produce: an
/// inert JSON result, or — for `daemon.shutdown` — a request that the
/// connection writes a final notification before closing.
pub enum DispatchOutcome {
    Result(Value),
    Shutdown(Value),
}

// Params types the table below names. Importing them keeps each row
// on one line; the module each belongs to is the module that owns the
// handler beside it.
use corpus::CorpusRebuildParams;
use dryrun::DryrunParams;
use glean::GleanSubmitParams;
use ingest::IngestSubmitParams;
use intake::IntakeOcrParams;
use libraries::LibraryForkParams;
use metadata::{
    MetadataAckParams, MetadataAdvanceParams, MetadataApproveParams, MetadataClearParams,
    MetadataContributorAddParams, MetadataContributorRemoveParams, MetadataReauditParams,
    MetadataRejectParams, MetadataSetParams, MetadataVoidParams,
};
use papers_corpus::PapersCorpusRebuildParams;
use papers_dryrun::PapersDryrunParams;
use papers_metadata::{
    PapersContributorAddParams, PapersContributorRemoveParams, PapersJustifiedReviewParams,
    PapersMetadataClearParams, PapersMetadataReauditParams, PapersMetadataSetParams,
    PapersMetadataVoidParams, PapersReviewParams,
};
use papers_remove::PapersRemoveParams;
use papers_stamps::ReconcileParams as PaperStampsReconcileParams;
use papers_vectors::{
    PapersVectorsDropParams, PapersVectorsRebuildParams, PapersVectorsReembedParams,
    PapersVectorsResetParams,
};
use reads::LibraryInfoParams;
use reads_library::{
    BookIdParams, FindBooksParams, FindPapersParams, LibraryOnlyParams, ListMetadataParams,
    PageParams, PaperAuditReadParams, ReadContextParams, ReadSpanParams, SearchInBookParams,
    SearchInPaperParams, SearchParams, ShowTocParams,
};
use remove::RemoveParams;
use stamps::ReconcileParams as BookStampsReconcileParams;
use vectors::{VectorsDropParams, VectorsRebuildParams, VectorsReembedParams, VectorsResetParams};
use verify::VerifyParams;

/// Single source of truth for every control-plane method.
///
/// Each row declares five facts about one method:
///
/// 1. `kind` — `read`, `write`, or `stream`; reflected in
///    `daemon.methods` so clients can pick the right call surface.
/// 2. `queue` — `queue` if the runtime routes the call through the
///    persistent queue worker (and so a headless `bookrack-mcp`
///    without `--with-queue-worker` must short-circuit it); otherwise
///    `no_queue`.
/// 3. `shape` — handler signature: `sync` for `fn(_, _) -> Result`,
///    `async` for `async fn(_, _) -> Result`, `sidebar` for methods
///    intercepted before `dispatch_normal` (the handler is left to a
///    hand-written arm in `dispatch`).
/// 4. `selection` — how an explicit library selection reaches the
///    method:
///    * `routed(<Params>)` — the method takes one, under the key the
///      params type's [`RoutedParams`] impl declares. Naming the type
///      is what makes the claim checkable rather than asserted.
///    * `process` — the method describes the process, not a library:
///      `daemon.*`, the queue verbs (whose job ids address one
///      daemon-wide queue), `logs.tail`, `tray.focus`, `diagnose.run`.
///      A selection is meaningless but harmless, so clients pass it
///      through unchanged.
///    * `unrouted` — the method answers about the daemon or about
///      every library at once, so a selection naming one library
///      cannot be honoured and must be refused rather than dropped.
/// 5. The method `name` and `=> handler` path (omitted for `sidebar`
///    entries).
///
/// The macro emits both the public `REGISTRY` const consumed by
/// `daemon.methods` / `daemon.mcp_tools` and the `dispatch_normal`
/// match table from this list, so the two tables cannot drift.
/// `is_queue_bound_method`, [`library_key_for`], and
/// [`refuses_library`] query `REGISTRY` directly for the same reason.
/// Sidebar rows still appear in `REGISTRY` but emit no arm in
/// `dispatch_normal`; their wire behaviour is implemented in
/// `dispatch` itself.
macro_rules! methods {
    (
        $( $kind:ident $queue:ident $shape:ident $selection:ident $( ( $params:path ) )?
           $name:literal $( => $handler:path )? ),* $(,)?
    ) => {
        pub const REGISTRY: &[meta::MethodSignature] = &[
            $(
                meta::MethodSignature {
                    name: $name,
                    kind: methods!(@kind $kind),
                    queue_bound: methods!(@queue $queue),
                    selection: methods!(@selection $selection),
                    library_key: methods!(@key $selection $( ( $params ) )?),
                },
            )*
        ];

        async fn dispatch_normal(
            method: &str,
            params: &Option<Value>,
            ctx: &MethodContext,
        ) -> Option<Result<Value, RpcError>> {
            $(
                methods!(@stmt $shape $name $( => $handler )?; method, params, ctx);
            )*
            None
        }
    };

    (@kind read)      => { "read" };
    (@kind write)     => { "write" };
    (@kind stream)    => { "stream" };

    (@queue queue)    => { true };
    (@queue no_queue) => { false };

    (@selection routed)   => { "routed" };
    (@selection process)  => { "process" };
    (@selection unrouted) => { "unrouted" };

    (@key routed ( $params:path )) => {
        Some(<$params as RoutedParams>::LIBRARY_KEY)
    };
    (@key process)  => { None };
    (@key unrouted) => { None };

    (@stmt sync $name:literal => $handler:path; $m:expr, $p:expr, $c:expr) => {
        if $m == $name {
            return Some($handler($p, $c));
        }
    };
    (@stmt async $name:literal => $handler:path; $m:expr, $p:expr, $c:expr) => {
        if $m == $name {
            return Some($handler($p, $c).await);
        }
    };
    (@stmt sidebar $name:literal; $_m:expr, $_p:expr, $_c:expr) => {
        // Sidebar methods are intercepted in `dispatch` before
        // `dispatch_normal` runs; no statement is emitted here.
    };
}

methods! {
    // daemon
    read  no_queue sync    process                    "daemon.version" => reads::daemon_version_rpc,
    write no_queue sidebar process                    "daemon.shutdown",
    read  no_queue sync    unrouted                   "status" => reads::status_rpc,
    read  no_queue sync    unrouted                   "daemon.status" => reads::status_rpc,
    read  no_queue async   unrouted                   "doctor.gather" => reads::doctor_gather_rpc,
    read  no_queue sync    process                    "daemon.methods" => meta::methods_rpc,
    read  no_queue sync    process                    "daemon.mcp_tools" => meta::mcp_tools_rpc,

    // queue
    read  no_queue sync    process                    "queue.list" => reads::queue_list,
    write no_queue async   process                    "queue.pause" => queue_writes::pause,
    write no_queue async   process                    "queue.resume" => queue_writes::resume,
    write no_queue async   process                    "queue.clear" => queue_writes::clear,

    // library admin
    read  no_queue sync    unrouted                   "library.list" => reads::library_list_rpc,
    read  no_queue async   routed(LibraryInfoParams)  "library.info" => reads::library_info,
    write no_queue async   routed(LibraryForkParams)  "library.fork" => libraries::fork,
    write no_queue async   unrouted
        "library.set_default" => libraries::set_default,
    write no_queue async   unrouted                   "library.mount" => libraries::mount,

    // library reads (sync, parametrised)
    read  no_queue sync    routed(LibraryOnlyParams)  "library.stats" => reads_library::stats,
    read  no_queue sync    routed(PageParams)
        "library.list_books" => reads_library::list_books,
    read  no_queue sync    routed(PageParams)
        "library.list_ocr_pending" => reads_library::list_ocr_pending,
    read  no_queue sync    routed(FindBooksParams)
        "library.find_books" => reads_library::find_books,
    read  no_queue sync    routed(BookIdParams)
        "library.show_book" => reads_library::show_book,
    read  no_queue sync    routed(ShowTocParams)      "library.show_toc" => reads_library::show_toc,
    read  no_queue sync    routed(ReadContextParams)
        "library.read_context" => reads_library::read_context,
    read  no_queue sync    routed(ReadSpanParams)
        "library.read_span" => reads_library::read_span,
    read  no_queue sync    routed(BookIdParams)
        "library.show_metadata_audit" => reads_library::show_metadata_audit,
    read  no_queue sync    routed(BookIdParams)
        "library.show_metadata_report" => reads_library::show_metadata_report,
    read  no_queue sync    routed(ListMetadataParams)
        "library.list_metadata" => reads_library::list_metadata,
    read  no_queue sync    routed(PageParams)
        "library.list_pending_reviews" => reads_library::list_pending_reviews,
    read  no_queue sync    routed(BookIdParams)
        "library.show_audit_trail" => reads_library::show_audit_trail,
    read  no_queue sync    routed(BookIdParams)
        "library.show_pipeline_trail" => reads_library::show_pipeline_trail,
    read  no_queue sync    routed(PageParams)
        "library.list_papers" => reads_library::list_papers,
    read  no_queue sync    routed(FindPapersParams)
        "library.find_papers" => reads_library::find_papers,
    read  no_queue sync    routed(BookIdParams)
        "library.show_paper" => reads_library::show_paper,
    read  no_queue sync    routed(ShowTocParams)
        "library.show_paper_toc" => reads_library::show_paper_toc,
    read  no_queue sync    routed(PaperAuditReadParams)
        "library.show_paper_metadata_report" => reads_library::show_paper_metadata_report,
    read  no_queue sync    routed(BookIdParams)
        "library.show_paper_audit_trail" => reads_library::show_paper_audit_trail,
    read  no_queue sync    routed(ListMetadataParams)
        "library.list_paper_metadata" => reads_library::list_paper_metadata,
    read  no_queue sync    routed(PageParams)
        "library.list_paper_pending_reviews" => reads_library::list_paper_pending_reviews,
    read  no_queue sync    routed(BookIdParams)
        "papers.export_csl" => reads_library::papers_export_csl,
    read  no_queue sync    routed(BookIdParams)
        "papers.fetch_source" => reads_library::papers_fetch_source,

    // library reads (async)
    read  no_queue async   routed(SearchParams)       "library.search" => reads_library::search,
    read  no_queue async   routed(SearchInBookParams)
        "library.search_in_book" => reads_library::search_in_book,
    read  no_queue async   routed(SearchInPaperParams)
        "library.search_in_paper" => reads_library::search_in_paper,
    read  no_queue async   routed(LibraryOnlyParams)
        "library.vectors_status" => reads_library::vectors_status,

    // events
    stream no_queue sidebar process                    "events.subscribe",
    read  no_queue sync    unrouted                   "events.snapshot" => reads::events_snapshot,

    // ingest / glean / intake
    write queue    async   routed(IngestSubmitParams) "ingest.submit" => ingest::submit,
    write queue    async   process                    "ingest.cancel" => ingest::cancel,
    write queue    async   routed(GleanSubmitParams)  "glean.submit" => glean::submit,
    write queue    async   routed(IntakeOcrParams)    "intake.ocr" => intake::submit,

    // book metadata curation
    write no_queue async   routed(MetadataSetParams)  "metadata.set" => metadata::set,
    write no_queue async   routed(MetadataClearParams) "metadata.clear" => metadata::clear,
    write no_queue async   routed(MetadataVoidParams) "metadata.void" => metadata::void,
    write no_queue async   routed(MetadataReauditParams) "metadata.reaudit" => metadata::reaudit,
    write no_queue async   routed(MetadataContributorAddParams)
        "metadata.contributor_add" => metadata::contributor_add,
    write no_queue async   routed(MetadataContributorRemoveParams)
        "metadata.contributor_remove" => metadata::contributor_remove,
    write no_queue async   routed(MetadataAckParams)  "metadata.ack" => metadata::ack,
    write no_queue async   routed(MetadataApproveParams) "metadata.approve" => metadata::approve,
    write no_queue async   routed(MetadataRejectParams) "metadata.reject" => metadata::reject,
    write queue    async   routed(MetadataAdvanceParams) "metadata.advance" => metadata::advance,

    // book vectors / corpus / stamps
    write queue    async   routed(VectorsRebuildParams) "vectors.rebuild" => vectors::rebuild,
    write queue    async   routed(VectorsReembedParams) "vectors.reembed" => vectors::reembed,
    write queue    async   routed(VectorsResetParams) "vectors.reset" => vectors::reset,
    write queue    async   routed(VectorsDropParams)  "vectors.drop" => vectors::drop_index,
    write queue    async   routed(CorpusRebuildParams) "corpus.rebuild" => corpus::rebuild,
    write queue    async   routed(BookStampsReconcileParams)
        "stamps.reconcile" => stamps::reconcile,

    // remove / dryrun (books)
    write queue    async   routed(RemoveParams)       "remove" => remove::run,
    write queue    async   routed(DryrunParams)       "dryrun" => dryrun::run,

    // paper maintenance triplet
    write queue    async   routed(PapersRemoveParams) "papers.remove" => papers_remove::run,
    write queue    async   routed(PapersCorpusRebuildParams)
        "papers.corpus_rebuild" => papers_corpus::rebuild,
    write queue    async   routed(PapersVectorsRebuildParams)
        "papers.vectors_rebuild" => papers_vectors::rebuild,
    write queue    async   routed(PapersVectorsReembedParams)
        "papers.vectors_reembed" => papers_vectors::reembed,
    write queue    async   routed(PapersVectorsResetParams)
        "papers.vectors_reset" => papers_vectors::reset,
    write queue    async   routed(PapersVectorsDropParams)
        "papers.vectors_drop" => papers_vectors::drop_index,
    write queue    async   routed(PaperStampsReconcileParams)
        "papers.stamps_reconcile" => papers_stamps::reconcile,
    write queue    async   routed(PapersDryrunParams) "papers.dryrun" => papers_dryrun::run,

    // paper metadata curation
    write no_queue async   routed(PapersMetadataReauditParams)
        "papers.metadata.reaudit" => papers_metadata::reaudit,
    write no_queue async   routed(PapersMetadataSetParams)
        "papers.metadata.set" => papers_metadata::set,
    write no_queue async   routed(PapersMetadataClearParams)
        "papers.metadata.clear" => papers_metadata::clear,
    write no_queue async   routed(PapersMetadataVoidParams)
        "papers.metadata.void" => papers_metadata::void,
    write no_queue async   routed(PapersJustifiedReviewParams)
        "papers.metadata.ack" => papers_metadata::ack,
    write no_queue async   routed(PapersReviewParams)
        "papers.metadata.approve" => papers_metadata::approve,
    write no_queue async   routed(PapersJustifiedReviewParams)
        "papers.metadata.reject" => papers_metadata::reject,
    write no_queue async   routed(PapersReviewParams)
        "papers.metadata.reopen" => papers_metadata::reopen,
    write no_queue async   routed(PapersContributorAddParams)
        "papers.metadata.contributor_add" => papers_metadata::contributor_add,
    write no_queue async   routed(PapersContributorRemoveParams)
        "papers.metadata.contributor_remove" => papers_metadata::contributor_remove,

    // verify / diagnose / tray / logs
    read  no_queue async   routed(VerifyParams)       "verify.run" => verify::run_rpc,
    read  no_queue async   process                    "diagnose.run" => diagnose::run,
    write no_queue sync    process                    "tray.focus" => tray::focus_rpc,
    read  no_queue sync    process                    "logs.tail" => logs::tail,
}

/// Method router. Method names are matched verbatim against the table
/// in [`docs/control-plane.md`](../../../../docs/control-plane.md).
pub async fn dispatch(req: &Request, ctx: &MethodContext) -> Result<DispatchOutcome, RpcError> {
    if !ctx.queue_worker_enabled && is_queue_bound_method(req.method.as_str()) {
        return Err(RpcError::new(
            QUEUE_WORKER_DISABLED,
            "queue worker disabled in headless mode".to_string(),
        ));
    }

    // Sidebar: methods whose handler shape does not fit
    // `dispatch_normal` (a non-`Result` outcome or an inline literal).
    // These names also appear in `REGISTRY` so `daemon.methods`
    // enumerates them; `sidebar_methods_appear_in_registry` enforces
    // that.
    match req.method.as_str() {
        "daemon.shutdown" => {
            return Ok(DispatchOutcome::Shutdown(reads::daemon_shutdown(ctx)));
        }
        "events.subscribe" => {
            return Ok(DispatchOutcome::Result(
                serde_json::json!({ "subscribed": true }),
            ));
        }
        _ => {}
    }

    match dispatch_normal(req.method.as_str(), &req.params, ctx).await {
        Some(result) => result.map(DispatchOutcome::Result),
        None => Err(RpcError::new(
            METHOD_NOT_FOUND,
            unknown_method_message(req.method.as_str(), &ctx.mcp_tools),
        )),
    }
}

/// Message for a method name [`dispatch_normal`] does not route. When
/// the name matches an MCP endpoint tool, say so — those tools are
/// reachable only from an MCP client, not over the control plane — and,
/// for the queue snapshot, name the control-plane equivalent. Kept
/// client-neutral: it points at `queue.list`, not any one front end's
/// spelling of it.
fn unknown_method_message(method: &str, mcp_tools: &[meta::McpToolInfo]) -> String {
    if !mcp_tools.iter().any(|t| t.name == method) {
        return format!("unknown method: {method}");
    }
    match method {
        "session.queue_status" => format!(
            "`{method}` is an MCP endpoint tool, not a control-plane method; \
             the control-plane queue snapshot is `queue.list`"
        ),
        _ => format!(
            "`{method}` is an MCP endpoint tool, not a control-plane method; \
             it is reachable only from an MCP client"
        ),
    }
}

/// JSON-RPC application code returned when the daemon cannot honour a
/// queue-bound write because the queue worker was not spawned.
/// Stable: callers (`bookrack-mcp` clients, the CLI) match on it to
/// distinguish a misconfigured headless entry from a transient busy
/// state.
// setting: internal -- a JSON-RPC application code callers match on, not a value to tune
pub const QUEUE_WORKER_DISABLED: i32 = -32002;

/// Returns `true` when the method routes work through the persistent
/// queue worker. Backed by `REGISTRY.queue_bound`, which the
/// `methods!` macro emits in lockstep with the dispatch arm — the two
/// cannot drift.
fn is_queue_bound_method(method: &str) -> bool {
    REGISTRY
        .iter()
        .any(|sig| sig.name == method && sig.queue_bound)
}

/// The params key under which `method` takes a library selection, or
/// `None` when it takes none.
///
/// The client side of the control plane injects the operator's
/// selection by this key rather than by a table of its own: the fact
/// belongs to the handler that reads it, and a second copy is a second
/// thing to keep true. An unknown method answers `None` — `rpc call`
/// forwards any name the caller types, and inventing a parameter for a
/// method this build does not have would put words in the caller's
/// mouth.
pub fn library_key_for(method: &str) -> Option<&'static str> {
    REGISTRY
        .iter()
        .find(|sig| sig.name == method)
        .and_then(|sig| sig.library_key)
}

/// Whether `method` cannot honour a library selection at all, so a
/// client holding an explicit one must refuse the call rather than
/// send it and let the selection evaporate.
///
/// False for a method this build does not know, for the same reason
/// [`library_key_for`] answers `None`.
pub fn refuses_library(method: &str) -> bool {
    REGISTRY
        .iter()
        .any(|sig| sig.name == method && sig.selection == "unrouted")
}

/// RAII bundle owning the write mutex guard and the broadcast handle
/// that raised the RPC write source / `McpAvailability { paused:
/// true }`. Its [`Drop`] publishes `mcp.availability { paused: false }`
/// and releases the write source — the state resolver falls back to
/// whatever other activity is live — before releasing the mutex, so
/// the state transitions cannot leak when the surrounding future is
/// cancelled or the blocking work panics mid-handler.
struct WriteSession {
    event_stream: EventStreamHandle,
    _guard: OwnedMutexGuard<()>,
}

impl Drop for WriteSession {
    fn drop(&mut self) {
        self.event_stream
            .publish(Event::McpAvailability { paused: false });
        self.event_stream.set_rpc_write(false);
        // _guard releases the write mutex after this body returns, so a
        // subsequent writer observes the released write source before
        // being able to take the lock.
    }
}

/// Acquire the runtime-wide write mutex, raise the RPC write source
/// (the daemon reads [`super::events::DaemonState::Writing`] for the
/// session's duration), broadcast `mcp.availability { paused: true }`,
/// run `op`
/// on a blocking executor, then unwind the broadcast and state
/// transitions in reverse. Concurrent writers see `-32001 busy`
/// instead of blocking on the mutex so the caller can retry.
///
/// `op` is driven on [`tokio::task::spawn_blocking`] with the current
/// runtime's [`tokio::runtime::Handle::block_on`] so handler bodies
/// can hold non-`Send` resources (the catalog and corpus handles use
/// `RefCell` internally) across `await` points without poisoning the
/// per-connection task that runs the dispatcher.
///
/// The state transitions and mutex release are tied to a
/// [`WriteSession`] RAII guard that is moved into the blocking task,
/// so cancellation of the dispatcher future or a panic inside `op`
/// cannot leave the daemon stranded with the write source raised and
/// MCP paused, or allow a second writer to enter while the blocking
/// work is still running.
pub(crate) async fn run_write<F, Fut>(
    ctx: &MethodContext,
    library: &str,
    op: F,
) -> Result<Value, RpcError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Value, RpcError>>,
{
    let guard = ctx
        .write_guard
        .clone()
        .try_lock_owned()
        .map_err(|_| RpcError::new(BUSY, "another write command is already in progress"))?;
    ctx.event_stream.set_rpc_write(true);
    ctx.event_stream
        .publish(Event::McpAvailability { paused: true });
    let session = WriteSession {
        event_stream: ctx.event_stream.clone(),
        _guard: guard,
    };
    let event_stream = ctx.event_stream.clone();
    let library_name = library.to_string();
    let join = tokio::task::spawn_blocking(move || {
        let handle = tokio::runtime::Handle::current();
        let result = handle.block_on(op());
        if result.is_ok() {
            event_stream.publish(Event::LibraryChanged {
                library: library_name,
            });
        }
        drop(session);
        result
    })
    .await;
    match join {
        Ok(result) => result,
        Err(e) => Err(RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            format!("write command join failed: {e}"),
        )),
    }
}

/// Workspace path forwarded into the dispatcher's selection. Exposed
/// for tests that want to fabricate a [`MethodContext`].
#[allow(dead_code)]
pub fn selection_data_dir(selection: &LibrarySelection) -> Option<&PathBuf> {
    selection.data_dir.as_ref()
}

/// Reject a destructive RPC with [`CONFIRMATION_REQUIRED`] unless the
/// caller explicitly opted in with `yes = true` or the request takes
/// a non-destructive path (`dry_run`, `resume`, ...).
///
/// The control plane never prompts on the client's behalf: every
/// destructive method that exposes a `yes` parameter routes through
/// this gate before any cmd-layer work runs.
pub(crate) fn require_yes(method: &str, yes: bool, exempt: bool) -> Result<(), RpcError> {
    if yes || exempt {
        return Ok(());
    }
    Err(RpcError::new(
        CONFIRMATION_REQUIRED,
        format!(
            "{method} requires `yes = true`: the control plane never prompts on \
             the caller's behalf. Confirm the destructive operation on the \
             client side, then resend with `yes = true`."
        ),
    ))
}

/// Build a [`MethodContext`] over a catalog-only ops handle rooted at
/// `dir`, so no embedder probe runs. `library_name` is the registry name
/// of the served library, `None` for a path-selected root. Shared by the
/// handler test modules so they all drive the same shape of context.
#[cfg(test)]
pub(crate) fn test_method_context(
    dir: &std::path::Path,
    library_name: Option<&str>,
) -> MethodContext {
    use bookrack_ops::registry::LibraryHandle;
    use bookrack_ops::{Caller, Ops};

    use crate::control::events::{DaemonState, DaemonStateFlag};

    let ops = Ops::<OllamaEmbedClient>::catalog_only(
        dir.join("corpus.db"),
        dir.join("catalog.db"),
        &dir.join("lancedb"),
        dir.join("books"),
        dir.join("backup"),
        Caller::cli(),
    );
    let cfg = Arc::new(Config::new(
        dir.to_path_buf(),
        "http://127.0.0.1:11434".to_string(),
    ));
    let handle = LibraryHandle::new(library_name.unwrap_or("default"), Arc::clone(&cfg), ops);
    let state = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
    let (shutdown_tx, _) = broadcast::channel(8);
    MethodContext {
        registry: LibraryRegistry::single(handle),
        info_context: LibraryInfoContext {
            data_dir: dir.display().to_string(),
            library_name: library_name.map(str::to_string),
            resolution_source: "explicit".to_string(),
            shadowed_default: None,
            library_identification: None,
            ollama_url: "http://127.0.0.1:11434".to_string(),
            embed_model_configured: "test-model".to_string(),
            mcp_addr: String::new(),
        },
        queue_state: Arc::new(Mutex::new(QueueState::default())),
        queue_state_path: dir.join("queue.json"),
        event_stream: EventStreamHandle::new(8, state),
        write_guard: Arc::new(TokioMutex::new(())),
        shutdown_tx,
        started_at_rfc3339: "2026-01-01T00:00:00Z".to_string(),
        selection: LibrarySelection::default(),
        library_name: library_name.unwrap_or("default").to_string(),
        mcp_tools: Arc::new(Vec::new()),
        queue_worker_enabled: false,
        tray_focus_signal: Arc::new(Notify::new()),
        rerank_supervisor: None,
        control_socket: None,
        queue_paused: Arc::new(AtomicBool::new(false)),
        log_stream: LogStreamHandle::new(8, 8),
        plan_registry: Arc::new(PlanRegistry::new()),
        mounter: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::events::{DaemonState, DaemonStateFlag};

    #[tokio::test]
    async fn write_session_drop_resets_state_and_releases_mutex() {
        let state = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let stream = EventStreamHandle::new(8, state.clone());
        let mut rx = stream.subscribe();

        let mutex = Arc::new(TokioMutex::new(()));
        let guard = mutex.clone().try_lock_owned().expect("lock free");
        stream.set_rpc_write(true);
        stream.publish(Event::McpAvailability { paused: true });
        assert_eq!(state.load(), DaemonState::Writing);

        let session = WriteSession {
            event_stream: stream.clone(),
            _guard: guard,
        };
        // A second writer cannot take the lock while the session lives.
        assert!(mutex.clone().try_lock_owned().is_err());
        drop(session);

        assert_eq!(state.load(), DaemonState::Idle);
        // The mutex is released, so a fresh owned lock is immediately
        // available to the next writer.
        assert!(mutex.clone().try_lock_owned().is_ok());

        // Drain the broadcast and assert the release events were emitted
        // in the order the daemon contract requires (paused:false then
        // DaemonState::Idle).
        let mut saw_unpaused = false;
        let mut saw_idle = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::McpAvailability { paused: false } => {
                    saw_unpaused = true;
                }
                Event::DaemonState(DaemonState::Idle) if saw_unpaused => {
                    saw_idle = true;
                }
                _ => {}
            }
        }
        assert!(saw_unpaused, "expected McpAvailability paused:false");
        assert!(saw_idle, "expected DaemonState::Idle after paused:false");
    }

    #[test]
    fn queue_bound_method_set_matches_dispatch_table() {
        for name in [
            "ingest.submit",
            "ingest.cancel",
            "glean.submit",
            "intake.ocr",
            "vectors.rebuild",
            "vectors.reembed",
            "vectors.reset",
            "vectors.drop",
            "corpus.rebuild",
            "stamps.reconcile",
            "remove",
            "papers.remove",
            "metadata.advance",
            "dryrun",
        ] {
            assert!(is_queue_bound_method(name), "{name} should be queue-bound");
        }
    }

    #[test]
    fn non_queue_methods_are_not_short_circuited() {
        for name in [
            "daemon.version",
            "daemon.shutdown",
            "daemon.methods",
            "daemon.mcp_tools",
            "status",
            "doctor.gather",
            "queue.list",
            "library.list",
            "library.info",
            "library.fork",
            "events.subscribe",
            "events.snapshot",
            "metadata.set",
            "metadata.clear",
            "metadata.ack",
            "metadata.approve",
            "metadata.reject",
            // The paper-side peers of the five above. Named here
            // because no list named them before, which is how they
            // drifted into the queue-bound column nothing on their
            // path ever used.
            "papers.metadata.reaudit",
            "papers.metadata.set",
            "papers.metadata.clear",
            "papers.metadata.void",
            "papers.metadata.ack",
            "papers.metadata.approve",
            "papers.metadata.reject",
            "papers.metadata.reopen",
            "papers.metadata.contributor_add",
            "papers.metadata.contributor_remove",
            "verify.run",
            "diagnose.run",
            "tray.focus",
        ] {
            assert!(
                !is_queue_bound_method(name),
                "{name} should pass through dispatch in headless mode"
            );
        }
    }

    #[test]
    fn queue_worker_disabled_code_is_stable() {
        assert_eq!(QUEUE_WORKER_DISABLED, -32002);
    }

    fn mcp_tool(name: &str) -> meta::McpToolInfo {
        meta::McpToolInfo {
            name: name.to_string(),
            description: String::new(),
        }
    }

    #[test]
    fn unknown_method_message_steers_queue_status_to_queue_list() {
        let tools = [mcp_tool("session.queue_status")];
        let msg = unknown_method_message("session.queue_status", &tools);
        assert!(msg.contains("MCP endpoint tool"), "{msg}");
        assert!(msg.contains("queue.list"), "{msg}");
    }

    #[test]
    fn unknown_method_message_generic_mcp_tool_omits_queue_list() {
        let tools = [mcp_tool("session.logs_tail")];
        let msg = unknown_method_message("session.logs_tail", &tools);
        assert!(msg.contains("MCP endpoint tool"), "{msg}");
        assert!(!msg.contains("queue.list"), "{msg}");
    }

    #[test]
    fn unknown_method_message_keeps_plain_form_for_truly_unknown() {
        let tools = [mcp_tool("session.queue_status")];
        let msg = unknown_method_message("library.no_such_read", &tools);
        assert_eq!(msg, "unknown method: library.no_such_read");
    }

    /// Names of every method intercepted by `dispatch` before
    /// `dispatch_normal` (because their handler shape does not fit the
    /// macro). Kept in lockstep with the sidebar match in `dispatch`
    /// by `sidebar_methods_appear_in_registry` below.
    const SIDEBAR_METHODS: &[&str] = &["daemon.shutdown", "events.subscribe"];

    #[test]
    fn sidebar_methods_appear_in_registry() {
        for name in SIDEBAR_METHODS {
            assert!(
                REGISTRY.iter().any(|sig| sig.name == *name),
                "sidebar method {name} must be added to REGISTRY so \
                 daemon.methods enumerates it"
            );
        }
    }

    #[test]
    fn sidebar_methods_are_not_queue_bound() {
        for name in SIDEBAR_METHODS {
            assert!(
                !is_queue_bound_method(name),
                "sidebar method {name} is intercepted before the queue-bound \
                 short-circuit, so marking it queue_bound has no effect and \
                 only confuses readers"
            );
        }
    }

    #[test]
    fn require_yes_rejects_default_request() {
        let err = require_yes("vectors.reset", false, false).unwrap_err();
        assert_eq!(err.code, CONFIRMATION_REQUIRED);
        assert!(err.message.contains("vectors.reset"));
        assert!(err.message.contains("yes = true"));
    }

    #[test]
    fn require_yes_admits_explicit_consent() {
        assert!(require_yes("vectors.reset", true, false).is_ok());
    }

    #[test]
    fn require_yes_admits_exempt_path() {
        assert!(require_yes("vectors.reembed", false, true).is_ok());
        assert!(require_yes("vectors.reset", false, true).is_ok());
    }

    #[test]
    fn require_yes_uses_distinct_error_code() {
        assert_ne!(CONFIRMATION_REQUIRED, super::super::jsonrpc::INVALID_PARAMS);
        assert_ne!(CONFIRMATION_REQUIRED, super::super::jsonrpc::INTERNAL_ERROR);
        assert_eq!(CONFIRMATION_REQUIRED, -32012);
    }
}
