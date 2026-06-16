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

use bookrack_config::{Config, LibrarySelection};
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_obs::stream::LogStreamHandle;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::LibraryRegistry;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, Notify, broadcast};

use super::events::{DaemonState, Event, EventStreamHandle};
use super::jsonrpc::{BUSY, CONFIRMATION_REQUIRED, METHOD_NOT_FOUND, Request, RpcError};

pub mod corpus;
pub mod diagnose;
pub mod dryrun;
pub mod glean;
pub mod ingest;
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

/// Read-mostly handles the dispatcher reaches into. The runtime owns
/// the originals; the dispatcher only clones cheap shared handles.
#[derive(Clone)]
pub struct MethodContext {
    pub cfg: Arc<Config>,
    pub registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    pub info_context: LibraryInfoContext,
    pub queue_state: Arc<Mutex<QueueState>>,
    pub queue_state_path: PathBuf,
    pub event_stream: EventStreamHandle,
    pub write_guard: Arc<TokioMutex<()>>,
    pub shutdown_tx: broadcast::Sender<()>,
    pub started_at_rfc3339: String,
    pub selection: LibrarySelection,
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
}

/// One of two terminal outcomes a method handler can produce: an
/// inert JSON result, or — for `daemon.shutdown` — a request that the
/// connection writes a final notification before closing.
pub enum DispatchOutcome {
    Result(Value),
    Shutdown(Value),
}

/// Single source of truth for every control-plane method.
///
/// Each row declares four facts about one method:
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
/// 4. The method `name` and `=> handler` path (omitted for `sidebar`
///    entries).
///
/// The macro emits both the public `REGISTRY` const consumed by
/// `daemon.methods` / `daemon.mcp_tools` and the `dispatch_normal`
/// match table from this list, so the two tables cannot drift.
/// `is_queue_bound_method` queries `REGISTRY` directly for the same
/// reason. Sidebar rows still appear in `REGISTRY` but emit no arm in
/// `dispatch_normal`; their wire behaviour is implemented in
/// `dispatch` itself.
macro_rules! methods {
    (
        $( $kind:ident $queue:ident $shape:ident $name:literal $( => $handler:path )? ),* $(,)?
    ) => {
        pub const REGISTRY: &[meta::MethodSignature] = &[
            $(
                meta::MethodSignature {
                    name: $name,
                    kind: methods!(@kind $kind),
                    queue_bound: methods!(@queue $queue),
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
    read   no_queue sync    "daemon.version"     => reads::daemon_version_rpc,
    write  no_queue sidebar "daemon.shutdown",
    read   no_queue sync    "status"             => reads::status_rpc,
    read   no_queue async   "doctor.gather"      => reads::doctor_gather_rpc,
    read   no_queue sync    "daemon.methods"     => meta::methods_rpc,
    read   no_queue sync    "daemon.mcp_tools"   => meta::mcp_tools_rpc,

    // queue
    read   no_queue sync    "queue.list"         => reads::queue_list,
    write  no_queue async   "queue.pause"        => queue_writes::pause,
    write  no_queue async   "queue.resume"       => queue_writes::resume,
    write  no_queue async   "queue.clear"        => queue_writes::clear,

    // library admin
    read   no_queue sync    "library.list"          => reads::library_list_rpc,
    read   no_queue async   "library.info"          => reads::library_info,
    write  no_queue async   "library.fork"          => libraries::fork,
    write  no_queue async   "library.set_default"   => libraries::set_default,

    // library reads (sync, parametrised)
    read   no_queue sync    "library.stats"                 => reads_library::stats,
    read   no_queue sync    "library.list_books"            => reads_library::list_books,
    read   no_queue sync    "library.find_books"            => reads_library::find_books,
    read   no_queue sync    "library.show_book"             => reads_library::show_book,
    read   no_queue sync    "library.show_toc"              => reads_library::show_toc,
    read   no_queue sync    "library.read_context"          => reads_library::read_context,
    read   no_queue sync    "library.read_span"             => reads_library::read_span,
    read   no_queue sync    "library.show_metadata_audit"   => reads_library::show_metadata_audit,
    read   no_queue sync    "library.show_metadata_report"  => reads_library::show_metadata_report,
    read   no_queue sync    "library.list_metadata"         => reads_library::list_metadata,
    read   no_queue sync    "library.list_pending_reviews"  => reads_library::list_pending_reviews,
    read   no_queue sync    "library.show_audit_trail"      => reads_library::show_audit_trail,
    read   no_queue sync    "library.show_pipeline_trail"   => reads_library::show_pipeline_trail,
    read   no_queue sync    "library.list_papers"           => reads_library::list_papers,
    read   no_queue sync    "library.find_papers"           => reads_library::find_papers,
    read   no_queue sync    "library.show_paper"            => reads_library::show_paper,
    read   no_queue sync    "library.show_paper_toc"        => reads_library::show_paper_toc,
    read   no_queue sync    "papers.export_csl"             => reads_library::papers_export_csl,
    read   no_queue sync    "papers.fetch_source"           => reads_library::papers_fetch_source,

    // library reads (async)
    read   no_queue async   "library.search"          => reads_library::search,
    read   no_queue async   "library.search_in_book"  => reads_library::search_in_book,
    read   no_queue async   "library.search_in_paper" => reads_library::search_in_paper,
    read   no_queue async   "library.vectors_status"  => reads_library::vectors_status,

    // events
    stream no_queue sidebar "events.subscribe",
    read   no_queue sync    "events.snapshot"     => reads::events_snapshot,

    // ingest / glean
    write  queue    async   "ingest.submit"       => ingest::submit,
    write  queue    async   "ingest.cancel"       => ingest::cancel,
    write  queue    async   "glean.submit"        => glean::submit,

    // book metadata curation
    write  no_queue async   "metadata.set"                => metadata::set,
    write  no_queue async   "metadata.clear"              => metadata::clear,
    write  no_queue async   "metadata.void"               => metadata::void,
    write  no_queue async   "metadata.reaudit"            => metadata::reaudit,
    write  no_queue async   "metadata.contributor_add"    => metadata::contributor_add,
    write  no_queue async   "metadata.contributor_remove" => metadata::contributor_remove,
    write  no_queue async   "metadata.ack"                => metadata::ack,
    write  no_queue async   "metadata.approve"            => metadata::approve,
    write  no_queue async   "metadata.reject"             => metadata::reject,
    write  queue    async   "metadata.advance"            => metadata::advance,

    // book vectors / corpus / stamps
    write  queue    async   "vectors.rebuild"     => vectors::rebuild,
    write  queue    async   "vectors.reembed"     => vectors::reembed,
    write  queue    async   "vectors.reset"       => vectors::reset,
    write  queue    async   "vectors.drop"        => vectors::drop_index,
    write  queue    async   "corpus.rebuild"      => corpus::rebuild,
    write  queue    async   "stamps.reconcile"    => stamps::reconcile,

    // remove / dryrun (books)
    write  queue    async   "remove"              => remove::run,
    write  queue    async   "dryrun"              => dryrun::run,

    // paper maintenance triplet
    write  queue    async   "papers.remove"             => papers_remove::run,
    write  queue    async   "papers.corpus_rebuild"     => papers_corpus::rebuild,
    write  queue    async   "papers.vectors_rebuild"    => papers_vectors::rebuild,
    write  queue    async   "papers.vectors_reembed"    => papers_vectors::reembed,
    write  queue    async   "papers.vectors_reset"      => papers_vectors::reset,
    write  queue    async   "papers.vectors_drop"       => papers_vectors::drop_index,
    write  queue    async   "papers.stamps_reconcile"   => papers_stamps::reconcile,
    write  queue    async   "papers.dryrun"             => papers_dryrun::run,

    // paper metadata curation
    write  queue    async   "papers.metadata.reaudit"            => papers_metadata::reaudit,
    write  queue    async   "papers.metadata.set"                => papers_metadata::set,
    write  queue    async   "papers.metadata.clear"              => papers_metadata::clear,
    write  queue    async   "papers.metadata.void"               => papers_metadata::void,
    write  queue    async   "papers.metadata.ack"                => papers_metadata::ack,
    write  queue    async   "papers.metadata.approve"            => papers_metadata::approve,
    write  queue    async   "papers.metadata.reject"             => papers_metadata::reject,
    write  queue    async   "papers.metadata.reopen"             => papers_metadata::reopen,
    write  queue    async   "papers.metadata.contributor_add"    => papers_metadata::contributor_add,
    write  queue    async   "papers.metadata.contributor_remove" => papers_metadata::contributor_remove,

    // verify / diagnose / tray / logs
    read   no_queue async   "verify.run"     => verify::run_rpc,
    read   no_queue async   "diagnose.run"   => diagnose::run,
    write  no_queue sync    "tray.focus"     => tray::focus_rpc,
    read   no_queue sync    "logs.tail"      => logs::tail,
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
            format!("unknown method: {}", req.method),
        )),
    }
}

/// JSON-RPC application code returned when the daemon cannot honour a
/// queue-bound write because the queue worker was not spawned.
/// Stable: callers (`bookrack-mcp` clients, the CLI) match on it to
/// distinguish a misconfigured headless entry from a transient busy
/// state.
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

/// Acquire the runtime-wide write mutex, flip the daemon into
/// [`DaemonState::Writing`], broadcast `mcp.availability { paused:
/// true }`, run `op` on a blocking executor, then unwind the
/// broadcast and state transitions in reverse. Concurrent writers see
/// `-32001 busy` instead of blocking on the mutex so the caller can
/// retry.
///
/// `op` is driven on [`tokio::task::spawn_blocking`] with the current
/// runtime's [`tokio::runtime::Handle::block_on`] so handler bodies
/// can hold non-`Send` resources (the catalog and corpus handles use
/// `RefCell` internally) across `await` points without poisoning the
/// per-connection task that runs the dispatcher.
pub(crate) async fn run_write<F, Fut>(ctx: &MethodContext, op: F) -> Result<Value, RpcError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Value, RpcError>>,
{
    let guard = ctx
        .write_guard
        .clone()
        .try_lock_owned()
        .map_err(|_| RpcError::new(BUSY, "another write command is already in progress"))?;
    ctx.event_stream.set_state(DaemonState::Writing);
    ctx.event_stream
        .publish(Event::McpAvailability { paused: true });
    let join = tokio::task::spawn_blocking(move || {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(op())
    })
    .await;
    let outcome = match join {
        Ok(result) => result,
        Err(e) => Err(RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            format!("write command join failed: {e}"),
        )),
    };
    ctx.event_stream.publish(Event::LibraryChanged {
        library: ctx.library_name.clone(),
    });
    ctx.event_stream
        .publish(Event::McpAvailability { paused: false });
    ctx.event_stream.set_state(DaemonState::Idle);
    drop(guard);
    outcome
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_bound_method_set_matches_dispatch_table() {
        for name in [
            "ingest.submit",
            "ingest.cancel",
            "glean.submit",
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
