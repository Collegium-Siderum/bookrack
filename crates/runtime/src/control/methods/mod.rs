// SPDX-License-Identifier: Apache-2.0

//! Control-plane method table.
//!
//! [`dispatch`] is the only entry point: hand it a parsed
//! [`Request`] and a [`MethodContext`], get back either the JSON
//! payload that becomes the response's `result` or an [`RpcError`].
//!
//! Two tables declare what the control plane serves. The methods that
//! act on one library — the library reads, the reference book's lookup
//! and overlay edit, the book-side metadata writes — are
//! [`bookrack_ops::methods::LIBRARY_METHODS`], whose bodies both
//! transports reach through the ops dispatcher. The host's own methods
//! are [`HOST_METHODS`], declared below. Reflection
//! ([`method_signatures`], [`library_key_for`], [`refuses_library`])
//! reads both.
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
use std::sync::{Arc, LazyLock, Mutex};

#[cfg(test)]
use bookrack_config::Config;
use bookrack_config::LibrarySelection;
use bookrack_core::DaemonIdentity;
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::stream::LogStreamHandle;
use bookrack_ops::OpsError;
use bookrack_ops::methods::{Access, LIBRARY_METHODS, MethodSpec, Selection, library_method};
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::write_session::{ChangeSet, Region, SessionRunError, WriteSession};
use serde_json::Value;
use tokio::sync::broadcast;

use super::error_map::rpc_from_problem;
use super::events::EventStreamHandle;
use super::jsonrpc::{CONFIRMATION_REQUIRED, INVALID_PARAMS, METHOD_NOT_FOUND, Request, RpcError};
use super::plan_registry::PlanRegistry;
use crate::cmd::input_error::CmdInputError;

/// A params type that carries a library selection: the trait the ops
/// method table checks its rows against, implemented here for the
/// host's own params types.
pub use bookrack_ops::methods::RoutedParams;

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
pub mod host_session;
pub mod ingest;
pub mod libraries;
mod library;
pub mod logs;
pub mod meta;
pub mod papers_corpus;
pub mod papers_dryrun;
pub mod papers_metadata;
pub mod papers_remove;
pub mod papers_stamps;
pub mod papers_vectors;
pub mod queue_writes;
pub mod reads;
pub mod remove;
pub mod session;
pub mod stamps;
pub mod vectors;
pub mod verify;

pub use reads::SNAPSHOT_CHANNELS;
pub use reads::snapshot_for;

/// Render a [`CmdInputError`] straight onto the wire envelope.
///
/// A handler that owns its own error mapping never hands an
/// `eyre::Report` to `handler_err`, so the cause-chain downcast there
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

/// The registry of libraries a daemon serves, over the embedder the
/// daemon runs.
pub type DaemonRegistry = LibraryRegistry<OllamaEmbedClient>;

/// Read-mostly handles the dispatcher reaches into. The runtime owns
/// the originals; the dispatcher only clones cheap shared handles.
#[derive(Clone)]
pub struct MethodContext {
    pub registry: Arc<DaemonRegistry>,
    pub info_context: LibraryInfoContext,
    pub queue_state: Arc<Mutex<QueueState>>,
    pub queue_state_path: PathBuf,
    pub event_stream: EventStreamHandle,
    pub shutdown_tx: broadcast::Sender<()>,
    /// Which daemon this is; reported by `daemon.version` and `status`.
    pub identity: DaemonIdentity,
    /// The process-level configuration the daemon was started with.
    pub daemon_config: Arc<bookrack_config::DaemonConfig>,
    /// What that configuration resolved to, row by row, as taken at
    /// start; `daemon.config` reports it.
    pub config_snapshot: Arc<crate::config_snapshot::ProcessSnapshot>,
    pub selection: LibrarySelection,
    /// Name of the primary (bring-up-selected) library. A single-value
    /// snapshot consumed by the `status` card and the `library.changed`
    /// snapshot a subscriber receives on connect; per-library status
    /// surfaces are a later milestone. It is not a routing fallback:
    /// a call that names no library resolves through
    /// [`select_library`], whose rule never consults this field.
    /// A `library.changed` published *by a write* names the library
    /// that write touched instead — [`run_write`] takes it from the
    /// handler's own handle.
    pub library_name: String,
    /// Registry names bring-up registered for the selected root, in
    /// the order written; empty when it registered nothing. A fact
    /// about this session, reported by `daemon.status` and not
    /// persisted anywhere.
    pub auto_registered: Vec<String>,
    /// Cached MCP tool list, populated by the daemon at startup from
    /// `bookrack_mcp::list_tools()`. Empty in entry points that do
    /// not bring up the MCP listener.
    pub mcp_tools: Arc<Vec<meta::McpToolInfo>>,
    /// `true` when the runtime spawned a queue worker. A host that
    /// spawns none leaves it `false`, in which case the
    /// dispatch routes queue-bound write methods to a
    /// `-32002 not_ready` response without invoking the handler.
    pub queue_worker_enabled: bool,
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
use libraries::LibraryForkParams;
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
use remove::RemoveParams;
use stamps::ReconcileParams as BookStampsReconcileParams;
use vectors::{VectorsDropParams, VectorsRebuildParams, VectorsReembedParams, VectorsResetParams};
use verify::VerifyParams;

/// Single source of truth for every control-plane method the host
/// serves itself; the library methods are the ops table's.
///
/// Each row declares five facts about one method:
///
/// 1. `kind` — `read`, `write`, or `stream`; reflected in
///    `daemon.methods` so clients can pick the right call surface.
/// 2. `queue` — a host that spawns no queue worker refuses the method
///    with `-32002` before it runs. Some of these methods enqueue work
///    for the worker (`ingest.submit`, `glean.submit`) or act on its
///    jobs (`ingest.cancel`, `queue.retry`); the others run
///    synchronously on the control plane, alongside whatever job the
///    worker is running. `no_queue` methods are served on every host.
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
///      daemon-wide queue), `logs.tail`. A selection is meaningless
///      but harmless, so clients pass it through unchanged. The test
///      is what the method gathers: a method whose answer carries one
///      library's data — `doctor.gather`'s store rows, `diagnose.run`'s
///      catalog snapshot — is routed, however much of the rest is
///      process-wide.
///    * `unrouted` — the method answers about the daemon or about
///      every library at once, so a selection naming one library
///      cannot be honoured and must be refused rather than dropped.
/// 5. The method `name` and `=> handler` path (omitted for `sidebar`
///    entries).
///
/// The macro emits both the public `HOST_METHODS` const and the
/// `dispatch_normal` match table from this list, so the two tables
/// cannot drift. [`method_signatures`] — what `daemon.methods` lists,
/// and what `is_queue_bound_method`, [`library_key_for`], and
/// [`refuses_library`] read — is these rows followed by the ops
/// table's. Sidebar rows still appear in `HOST_METHODS` but emit no arm
/// in `dispatch_normal`; their wire behaviour is implemented in
/// `dispatch` itself.
macro_rules! methods {
    (
        $( $kind:ident $queue:ident $shape:ident $selection:ident $( ( $params:path ) )?
           $name:literal $( => $handler:path )? ),* $(,)?
    ) => {
        pub const HOST_METHODS: &[meta::MethodSignature] = &[
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
    read  no_queue sync    unrouted                   "daemon.config" => reads::daemon_config_rpc,
    read  no_queue async   routed(reads::DoctorGatherParams)
        "doctor.gather" => reads::doctor_gather_rpc,
    read  no_queue sync    process                    "daemon.methods" => meta::methods_rpc,
    read  no_queue sync    process                    "daemon.mcp_tools" => meta::mcp_tools_rpc,

    // queue
    read  no_queue sync    process                    "queue.list" => reads::queue_list,
    write no_queue async   process                    "queue.pause" => queue_writes::pause,
    write no_queue async   process                    "queue.resume" => queue_writes::resume,
    write no_queue async   process                    "queue.clear" => queue_writes::clear,
    write queue    async   process                    "queue.retry" => queue_writes::retry,

    // library admin
    read  no_queue sync    unrouted                   "library.list" => reads::library_list_rpc,
    write no_queue async   routed(LibraryForkParams)  "library.fork" => libraries::fork,
    write no_queue async   unrouted
        "library.set_default" => libraries::set_default,
    write no_queue async   unrouted                   "library.mount" => libraries::mount,
    write no_queue async   unrouted                   "library.unmount" => libraries::unmount,

    // events
    stream no_queue sidebar process                    "events.subscribe",
    read  no_queue sync    unrouted                   "events.snapshot" => reads::events_snapshot,

    // ingest / glean
    write queue    async   routed(IngestSubmitParams) "ingest.submit" => ingest::submit,
    write queue    async   process                    "ingest.cancel" => ingest::cancel,
    write queue    async   routed(GleanSubmitParams)  "glean.submit" => glean::submit,

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

    // verify / diagnose / logs
    read  no_queue async   routed(VerifyParams)       "verify.run" => verify::run_rpc,
    read  no_queue async   routed(diagnose::DiagnoseRunParams)
        "diagnose.run" => diagnose::run,
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

    if let Some(spec) = library_method(req.method.as_str()) {
        return library::dispatch(spec, &req.params, ctx)
            .await
            .map(DispatchOutcome::Result);
    }

    // Sidebar: methods whose handler shape does not fit
    // `dispatch_normal` (a non-`Result` outcome or an inline literal).
    // These names also appear in `HOST_METHODS` so `daemon.methods`
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
/// Stable: callers (MCP clients, the CLI) match on it to distinguish
/// a host that spawned no queue worker from a transient busy state.
// setting: internal -- a JSON-RPC application code callers match on, not a value to tune
pub const QUEUE_WORKER_DISABLED: i32 = -32002;

/// Every method the control plane serves, as `daemon.methods` lists
/// it: the host's rows in declaration order, then the ops table's.
pub fn method_signatures() -> &'static [meta::MethodSignature] {
    static ALL: LazyLock<Vec<meta::MethodSignature>> = LazyLock::new(|| {
        HOST_METHODS
            .iter()
            .cloned()
            .chain(LIBRARY_METHODS.iter().map(library_signature))
            .collect()
    });
    &ALL
}

/// The `daemon.methods` row of one ops table method.
fn library_signature(spec: &MethodSpec) -> meta::MethodSignature {
    let (selection, library_key) = match spec.selection {
        Selection::Routed { control_key, .. } => ("routed", Some(control_key)),
        Selection::Process => ("process", None),
        Selection::Unrouted => ("unrouted", None),
    };
    meta::MethodSignature {
        name: spec.name,
        kind: match spec.access {
            Access::Read => "read",
            Access::Write { .. } | Access::Bookkeeping => "write",
            Access::Stream => "stream",
        },
        queue_bound: spec.needs_queue,
        selection,
        library_key,
    }
}

/// Returns `true` when a host that spawns no queue worker refuses the
/// method before it runs. Backed by the rows' `queue_bound`, which the
/// `methods!` macro emits in lockstep with the dispatch arm — the two
/// cannot drift.
fn is_queue_bound_method(method: &str) -> bool {
    method_signatures()
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
    method_signatures()
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
    method_signatures()
        .iter()
        .any(|sig| sig.name == method && sig.selection == "unrouted")
}

/// The handle a routed method acts on: the library `name` names, or,
/// when the caller named none, the one the registry routes an unnamed
/// call to — the registry default while it is mounted, the only
/// mounted library when no default is declared, and otherwise a
/// refusal. One lookup shared by every library-level method, so the
/// unnamed route is decided in one place, at the registry; a name the
/// registry does not hold and an unnamed call it cannot route are
/// both `-32010`.
pub fn select_library(
    ctx: &MethodContext,
    name: Option<&str>,
) -> Result<Arc<LibraryHandle<OllamaEmbedClient>>, RpcError> {
    ctx.registry
        .get(name)
        .map_err(super::error_map::registry_err)
}

/// The registry name a queue-submitting method writes into its job:
/// [`select_library`]'s answer for `name`, so an unnamed submit is
/// pinned to the library the registry routes it to at enqueue time,
/// and a name the registry does not hold or an unnamed submit it
/// cannot route is `-32010` before any row exists. The job carries the
/// resolved name from then on; moving the default pointer later does
/// not move a queued job.
pub fn resolve_submit_target(ctx: &MethodContext, name: Option<&str>) -> Result<String, RpcError> {
    Ok(select_library(ctx, name)?.name().to_string())
}

/// Whether a write session ends with a `library.changed` broadcast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Announce {
    /// The session may have changed the library: announce it.
    Changed,
    /// The session left every library as it found it: a dry run.
    Nothing,
}

/// Run a write command on `handle`'s library: a write session (see
/// [`run_library_session`]) that announces `library.changed` on
/// success, so every subscriber refreshes what it believes about the
/// library.
pub(crate) async fn run_write<F>(
    ctx: &MethodContext,
    handle: &Arc<LibraryHandle<OllamaEmbedClient>>,
    op: F,
) -> Result<Value, RpcError>
where
    F: AsyncFnOnce(&mut WriteSession<OllamaEmbedClient>) -> Result<Value, RpcError>
        + Send
        + 'static,
{
    run_library_session(ctx, handle, Announce::Changed, op).await
}

/// Run a dry-run leg: a write session (see [`run_library_session`])
/// that publishes no `library.changed`. The leg takes the write mutex
/// like the write it previews — the plan it registers is a snapshot
/// nothing may move under, and `mcp.availability` pauses and resumes
/// around it as around any holder of the mutex — but it leaves every
/// library as it found it, so there is nothing for a subscriber to
/// refresh.
pub(crate) async fn run_dry_run<F>(
    ctx: &MethodContext,
    handle: &Arc<LibraryHandle<OllamaEmbedClient>>,
    op: F,
) -> Result<Value, RpcError>
where
    F: AsyncFnOnce(&mut WriteSession<OllamaEmbedClient>) -> Result<Value, RpcError>
        + Send
        + 'static,
{
    run_library_session(ctx, handle, Announce::Nothing, op).await
}

/// Open a write session on `handle`'s library through the registry and
/// run `op` in it.
///
/// The registry's session takes the daemon's one write mutex without
/// waiting, so a concurrent writer sees `-32001 busy` and may retry; it
/// raises the RPC write source and pauses MCP through
/// [`host_session::DaemonChangeSink`] for the session's duration. `op`
/// runs on a blocking task with the session lent to it, so handler
/// bodies can hold non-`Send` resources (the catalog and corpus handles
/// use `RefCell` internally) across `await` points. The announcement and
/// the release happen on that same task, so cancelling the dispatcher
/// future or a panic inside `op` cannot leave MCP paused, the write
/// source raised, or a committed write unannounced.
///
/// A success reply that is a JSON object names the library the write
/// acted on: `library` is set to the handle's name unless `op` already
/// put a `library` key there. A reply of any other shape, and every
/// error, passes through unchanged. `announce` decides whether a
/// successful `op` ends with `library.changed { library }`.
async fn run_library_session<F>(
    ctx: &MethodContext,
    handle: &Arc<LibraryHandle<OllamaEmbedClient>>,
    announce: Announce,
    op: F,
) -> Result<Value, RpcError>
where
    F: AsyncFnOnce(&mut WriteSession<OllamaEmbedClient>) -> Result<Value, RpcError>
        + Send
        + 'static,
{
    let caller = handle.ops().caller().clone();
    let session = ctx
        .registry
        .begin_write(handle, Region::Library, caller)
        .map_err(|busy| super::error_map::ops_err(OpsError::WriteBusy(busy)))?;
    let library = handle.name().to_string();
    session
        .run_blocking(async move |s| {
            let value = name_the_library(op(s).await?, &library);
            let changes = match announce {
                Announce::Changed => ChangeSet::announced(library),
                Announce::Nothing => ChangeSet::unchanged(),
            };
            Ok((value, changes))
        })
        .await
        .map_err(session_failure)
}

/// `value` with a `library` key naming `library`, when it is a JSON
/// object that does not carry one already.
pub(super) fn name_the_library(mut value: Value, library: &str) -> Value {
    if let Value::Object(map) = &mut value {
        map.entry("library")
            .or_insert_with(|| Value::String(library.to_string()));
    }
    value
}

/// A session that did not settle as its write: the write's own error,
/// or the blocking task that ran it failing.
pub(super) fn session_failure(e: SessionRunError<RpcError>) -> RpcError {
    match e {
        SessionRunError::Op(rpc) => rpc,
        SessionRunError::Join(e) => RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            format!("write command join failed: {e}"),
        ),
        SessionRunError::RefreshStore(e) => RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            format!(
                "refresh vector store after write: {}",
                bookrack_core::error_chain(&e)
            ),
        ),
    }
}

/// A write's success reply built from the report its command returned:
/// the report's fields with `ok: true` beside them. [`run_write`] adds
/// `library` on the way out.
pub(crate) fn ok_reply<T: serde::Serialize>(report: &T) -> Result<Value, RpcError> {
    let mut value = serde_json::to_value(report).map_err(|e| {
        RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            format!("could not serialize the write reply: {e}"),
        )
    })?;
    let Some(map) = value.as_object_mut() else {
        return Err(RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            "a write report must serialize to a JSON object",
        ));
    };
    map.insert("ok".to_string(), Value::Bool(true));
    Ok(value)
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
    use crate::control::events::{DaemonState, DaemonStateFlag};

    let name = library_name.unwrap_or("default");
    let handle = test_library_handle(dir, name);
    let state = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
    let event_stream = EventStreamHandle::new(8, state);
    let (shutdown_tx, _) = broadcast::channel(8);
    MethodContext {
        registry: LibraryRegistry::from_handles_with_sink(
            [handle],
            Some(name.to_string()),
            Arc::new(host_session::DaemonChangeSink::new(event_stream.clone())),
        )
        .expect("a one-library registry"),
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
        event_stream,
        shutdown_tx,
        identity: DaemonIdentity {
            instance_id: "018f0000-0000-7000-8000-000000000000".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            runtime_dir: dir.display().to_string(),
            control_sock: None,
            started_at: "2026-01-01T00:00:00Z".to_string(),
        },
        daemon_config: Arc::new(bookrack_config::DaemonConfig::resolve(
            &bookrack_config::EnvSnapshot::default(),
            &bookrack_config::DaemonFlags {
                no_mcp: true,
                ..bookrack_config::DaemonFlags::default()
            },
        )),
        config_snapshot: Arc::default(),
        selection: LibrarySelection::default(),
        library_name: library_name.unwrap_or("default").to_string(),
        auto_registered: Vec::new(),
        mcp_tools: Arc::new(Vec::new()),
        queue_worker_enabled: false,
        rerank_supervisor: None,
        control_socket: None,
        queue_paused: Arc::new(AtomicBool::new(false)),
        log_stream: LogStreamHandle::new(8, 8),
        plan_registry: Arc::new(PlanRegistry::new()),
        mounter: None,
    }
}

/// A catalog-only library handle named `name` and rooted at `dir`, the
/// shape [`test_method_context`] serves; a test mounts a second one
/// beside it to put two libraries in the registry.
#[cfg(test)]
pub(crate) fn test_library_handle(
    dir: &std::path::Path,
    name: &str,
) -> Arc<bookrack_ops::registry::LibraryHandle<OllamaEmbedClient>> {
    use bookrack_ops::registry::LibraryHandle;
    use bookrack_ops::{Caller, Ops};

    let ops = Ops::<OllamaEmbedClient>::catalog_only(
        dir.join("corpus.db"),
        dir.join("catalog.db"),
        &dir.join("lancedb"),
        dir.join("books"),
        dir.join("backup"),
        Caller::cli(),
    );
    let cfg = Arc::new(Config::new(dir.to_path_buf()));
    LibraryHandle::new(name, cfg, ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::events::{DaemonState, Event};
    use serde_json::json;

    /// A registry session raises the write source and pauses MCP while
    /// it is open; ending it unpauses before the state falls back to
    /// idle, and only then frees the mutex for the next writer.
    #[tokio::test]
    async fn an_open_session_holds_the_daemon_in_writing_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), Some("alpha"));
        let mut rx = ctx.event_stream.subscribe();

        let session = ctx
            .registry
            .begin_host_write(bookrack_ops::Caller::control_plane(), "alpha", None)
            .expect("the mutex is free");
        assert_eq!(ctx.event_stream.current_state(), DaemonState::Writing);
        assert!(
            ctx.registry
                .begin_host_write(bookrack_ops::Caller::control_plane(), "alpha", None)
                .is_err(),
            "a second writer cannot open a session while the first lives"
        );
        drop(session);

        assert_eq!(ctx.event_stream.current_state(), DaemonState::Idle);
        assert!(
            ctx.registry
                .begin_host_write(bookrack_ops::Caller::control_plane(), "alpha", None)
                .is_ok(),
            "the mutex is free once the session ends"
        );

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

    /// Drive `op` through [`run_write`] on the handle `name` selects in
    /// a single-library context served under `served`, so the library
    /// name reaches `run_write` the way a handler hands it over.
    async fn write_reply(
        served: Option<&str>,
        name: Option<&str>,
        reply: Result<Value, RpcError>,
    ) -> Result<Value, RpcError> {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), served);
        let handle = select_library(&ctx, name).expect("the served library resolves");
        run_write(&ctx, &handle, async move |_session| reply).await
    }

    /// Every event a session published, in order, as `channel` plus its
    /// serialized value, with the daemon state transitions left out.
    fn session_events(rx: &mut broadcast::Receiver<Event>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if !matches!(event, Event::DaemonState(_)) {
                seen.push(format!("{} {}", event.channel(), event.value()));
            }
        }
        seen
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_pauses_mcp_and_announces_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), Some("alpha"));
        let mut rx = ctx.event_stream.subscribe();
        let handle = select_library(&ctx, Some("alpha")).expect("alpha resolves");

        run_write(&ctx, &handle, async move |_session| Ok(json!({})))
            .await
            .unwrap();

        let events = session_events(&mut rx);
        assert_eq!(
            events,
            vec![
                r#"mcp.availability {"paused":true}"#,
                r#"library.changed {"library":"alpha"}"#,
                r#"mcp.availability {"paused":false}"#,
            ],
            "{events:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dry_run_pauses_mcp_and_announces_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), Some("alpha"));
        let mut rx = ctx.event_stream.subscribe();
        let handle = select_library(&ctx, Some("alpha")).expect("alpha resolves");

        let reply = run_dry_run(&ctx, &handle, async move |_session| Ok(json!({})))
            .await
            .unwrap();
        assert_eq!(reply["library"], json!("alpha"), "{reply}");

        let events = session_events(&mut rx);
        assert_eq!(
            events,
            vec![
                r#"mcp.availability {"paused":true}"#,
                r#"mcp.availability {"paused":false}"#,
            ],
            "{events:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_write_names_the_library_on_a_bare_ok_reply() {
        let reply = write_reply(Some("alpha"), None, Ok(serde_json::json!({ "ok": true })))
            .await
            .expect("the write succeeds");
        assert_eq!(reply, serde_json::json!({ "ok": true, "library": "alpha" }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_write_adds_library_beside_existing_fields() {
        let reply = write_reply(
            Some("alpha"),
            Some("alpha"),
            Ok(serde_json::json!({ "name": "x" })),
        )
        .await
        .expect("the write succeeds");
        assert_eq!(reply["name"], "x", "{reply}");
        assert_eq!(reply["library"], "alpha", "{reply}");
    }

    /// A path-selected root is served under the handle name `default`,
    /// the same name `daemon.rs` gives an unnamed configuration; the
    /// reply reports that name as it is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_write_names_the_anonymous_root_default() {
        let reply = write_reply(None, None, Ok(serde_json::json!({ "ok": true })))
            .await
            .expect("the write succeeds");
        assert_eq!(reply["library"], "default", "{reply}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_write_keeps_an_existing_library_key() {
        let reply = write_reply(
            Some("alpha"),
            None,
            Ok(serde_json::json!({ "library": "keep" })),
        )
        .await
        .expect("the write succeeds");
        assert_eq!(reply, serde_json::json!({ "library": "keep" }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_write_leaves_a_non_object_reply_alone() {
        let reply = write_reply(Some("alpha"), None, Ok(serde_json::json!([1, 2])))
            .await
            .expect("the write succeeds");
        assert_eq!(reply, serde_json::json!([1, 2]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_write_does_not_touch_an_error_reply() {
        let err = write_reply(
            Some("alpha"),
            None,
            Err(RpcError::new(INVALID_PARAMS, "refused as given")),
        )
        .await
        .expect_err("the write fails");
        assert_eq!(err.code, INVALID_PARAMS);
        assert_eq!(err.message, "refused as given");
        assert!(err.data.is_none(), "{err:?}");
    }

    #[test]
    fn queue_bound_method_set_matches_dispatch_table() {
        for name in [
            "ingest.submit",
            "ingest.cancel",
            "queue.retry",
            "glean.submit",
            "vectors.rebuild",
            "vectors.reembed",
            "vectors.reset",
            "vectors.drop",
            "corpus.rebuild",
            "stamps.reconcile",
            "remove",
            "papers.remove",
            "dryrun",
        ] {
            assert!(is_queue_bound_method(name), "{name} should be queue-bound");
        }
    }

    /// Neither the OCR intake submission nor its worklist is a method.
    #[test]
    fn the_ocr_intake_methods_are_not_in_the_dispatch_table() {
        for name in ["intake.ocr", "library.list_ocr_pending"] {
            assert!(
                method_signatures().iter().all(|method| method.name != name),
                "{name} is still dispatched"
            );
            assert!(!is_queue_bound_method(name), "{name} is still queue-bound");
        }
    }

    /// `metadata.advance` is not a method.
    #[test]
    fn the_metadata_advance_method_is_not_in_the_dispatch_table() {
        assert!(
            method_signatures()
                .iter()
                .all(|method| method.name != "metadata.advance"),
            "metadata.advance is still dispatched"
        );
        assert!(
            !is_queue_bound_method("metadata.advance"),
            "metadata.advance is still queue-bound"
        );
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

    /// `tray.focus` is not registered and the router answers a call to
    /// it with the standard method-not-found error.
    #[tokio::test]
    async fn tray_focus_is_not_a_method() {
        assert!(
            !method_signatures()
                .iter()
                .any(|sig| sig.name == "tray.focus"),
            "tray.focus must not be a method"
        );
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), None);
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tray.focus".to_string(),
            params: None,
        };
        match dispatch(&req, &ctx).await {
            Err(err) => assert_eq!(err.code, METHOD_NOT_FOUND, "{err:?}"),
            Ok(_) => panic!("tray.focus must not dispatch"),
        }
    }

    #[test]
    fn sidebar_methods_appear_in_registry() {
        for name in SIDEBAR_METHODS {
            assert!(
                HOST_METHODS.iter().any(|sig| sig.name == *name),
                "sidebar method {name} must be added to HOST_METHODS so \
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
