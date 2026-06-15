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
use super::jsonrpc::{BUSY, METHOD_NOT_FOUND, Request, RpcError};

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

/// Method router. Method names are matched verbatim against the table
/// in [`docs/control-plane.md`](../../../../docs/control-plane.md).
pub async fn dispatch(req: &Request, ctx: &MethodContext) -> Result<DispatchOutcome, RpcError> {
    if !ctx.queue_worker_enabled && is_queue_bound_method(req.method.as_str()) {
        return Err(RpcError::new(
            QUEUE_WORKER_DISABLED,
            "queue worker disabled in headless mode".to_string(),
        ));
    }
    match req.method.as_str() {
        "daemon.version" => Ok(DispatchOutcome::Result(reads::daemon_version(ctx))),
        "daemon.shutdown" => Ok(DispatchOutcome::Shutdown(reads::daemon_shutdown(ctx))),
        "status" => Ok(DispatchOutcome::Result(reads::status(ctx))),
        "doctor.gather" => Ok(DispatchOutcome::Result(reads::doctor_gather(ctx).await)),
        "queue.list" => Ok(DispatchOutcome::Result(reads::queue_list(
            &req.params,
            ctx,
        )?)),
        "queue.pause" => Ok(DispatchOutcome::Result(
            queue_writes::pause(&req.params, ctx).await?,
        )),
        "queue.resume" => Ok(DispatchOutcome::Result(
            queue_writes::resume(&req.params, ctx).await?,
        )),
        "queue.clear" => Ok(DispatchOutcome::Result(
            queue_writes::clear(&req.params, ctx).await?,
        )),
        "library.list" => Ok(DispatchOutcome::Result(reads::library_list(ctx)?)),
        "library.info" => Ok(DispatchOutcome::Result(
            reads::library_info(&req.params, ctx).await?,
        )),
        "library.stats" => Ok(DispatchOutcome::Result(reads_library::stats(
            &req.params,
            ctx,
        )?)),
        "library.list_books" => Ok(DispatchOutcome::Result(reads_library::list_books(
            &req.params,
            ctx,
        )?)),
        "library.find_books" => Ok(DispatchOutcome::Result(reads_library::find_books(
            &req.params,
            ctx,
        )?)),
        "library.show_book" => Ok(DispatchOutcome::Result(reads_library::show_book(
            &req.params,
            ctx,
        )?)),
        "library.show_toc" => Ok(DispatchOutcome::Result(reads_library::show_toc(
            &req.params,
            ctx,
        )?)),
        "library.read_context" => Ok(DispatchOutcome::Result(reads_library::read_context(
            &req.params,
            ctx,
        )?)),
        "library.read_span" => Ok(DispatchOutcome::Result(reads_library::read_span(
            &req.params,
            ctx,
        )?)),
        "library.show_metadata_audit" => Ok(DispatchOutcome::Result(
            reads_library::show_metadata_audit(&req.params, ctx)?,
        )),
        "library.show_metadata_report" => Ok(DispatchOutcome::Result(
            reads_library::show_metadata_report(&req.params, ctx)?,
        )),
        "library.list_metadata" => Ok(DispatchOutcome::Result(reads_library::list_metadata(
            &req.params,
            ctx,
        )?)),
        "library.list_pending_reviews" => Ok(DispatchOutcome::Result(
            reads_library::list_pending_reviews(&req.params, ctx)?,
        )),
        "library.show_audit_trail" => Ok(DispatchOutcome::Result(reads_library::show_audit_trail(
            &req.params,
            ctx,
        )?)),
        "library.show_pipeline_trail" => Ok(DispatchOutcome::Result(
            reads_library::show_pipeline_trail(&req.params, ctx)?,
        )),
        "library.search" => Ok(DispatchOutcome::Result(
            reads_library::search(&req.params, ctx).await?,
        )),
        "library.search_in_book" => Ok(DispatchOutcome::Result(
            reads_library::search_in_book(&req.params, ctx).await?,
        )),
        "library.list_papers" => Ok(DispatchOutcome::Result(reads_library::list_papers(
            &req.params,
            ctx,
        )?)),
        "library.find_papers" => Ok(DispatchOutcome::Result(reads_library::find_papers(
            &req.params,
            ctx,
        )?)),
        "library.show_paper" => Ok(DispatchOutcome::Result(reads_library::show_paper(
            &req.params,
            ctx,
        )?)),
        "library.show_paper_toc" => Ok(DispatchOutcome::Result(reads_library::show_paper_toc(
            &req.params,
            ctx,
        )?)),
        "library.search_in_paper" => Ok(DispatchOutcome::Result(
            reads_library::search_in_paper(&req.params, ctx).await?,
        )),
        "papers.export_csl" => Ok(DispatchOutcome::Result(reads_library::papers_export_csl(
            &req.params,
            ctx,
        )?)),
        "papers.fetch_source" => Ok(DispatchOutcome::Result(reads_library::papers_fetch_source(
            &req.params,
            ctx,
        )?)),
        "library.vectors_status" => Ok(DispatchOutcome::Result(
            reads_library::vectors_status(&req.params, ctx).await?,
        )),
        "logs.tail" => Ok(DispatchOutcome::Result(logs::tail(&req.params, ctx)?)),
        "events.subscribe" => Ok(DispatchOutcome::Result(
            serde_json::json!({ "subscribed": true }),
        )),
        "events.snapshot" => Ok(DispatchOutcome::Result(reads::events_snapshot(
            &req.params,
            ctx,
        )?)),
        "ingest.submit" => Ok(DispatchOutcome::Result(
            ingest::submit(&req.params, ctx).await?,
        )),
        "ingest.cancel" => Ok(DispatchOutcome::Result(
            ingest::cancel(&req.params, ctx).await?,
        )),
        "glean.submit" => Ok(DispatchOutcome::Result(
            glean::submit(&req.params, ctx).await?,
        )),
        "metadata.set" => Ok(DispatchOutcome::Result(
            metadata::set(&req.params, ctx).await?,
        )),
        "metadata.clear" => Ok(DispatchOutcome::Result(
            metadata::clear(&req.params, ctx).await?,
        )),
        "metadata.void" => Ok(DispatchOutcome::Result(
            metadata::void(&req.params, ctx).await?,
        )),
        "metadata.reaudit" => Ok(DispatchOutcome::Result(
            metadata::reaudit(&req.params, ctx).await?,
        )),
        "metadata.contributor_add" => Ok(DispatchOutcome::Result(
            metadata::contributor_add(&req.params, ctx).await?,
        )),
        "metadata.contributor_remove" => Ok(DispatchOutcome::Result(
            metadata::contributor_remove(&req.params, ctx).await?,
        )),
        "metadata.ack" => Ok(DispatchOutcome::Result(
            metadata::ack(&req.params, ctx).await?,
        )),
        "metadata.approve" => Ok(DispatchOutcome::Result(
            metadata::approve(&req.params, ctx).await?,
        )),
        "metadata.reject" => Ok(DispatchOutcome::Result(
            metadata::reject(&req.params, ctx).await?,
        )),
        "metadata.advance" => Ok(DispatchOutcome::Result(
            metadata::advance(&req.params, ctx).await?,
        )),
        "vectors.rebuild" => Ok(DispatchOutcome::Result(
            vectors::rebuild(&req.params, ctx).await?,
        )),
        "vectors.reembed" => Ok(DispatchOutcome::Result(
            vectors::reembed(&req.params, ctx).await?,
        )),
        "vectors.reset" => Ok(DispatchOutcome::Result(
            vectors::reset(&req.params, ctx).await?,
        )),
        "vectors.drop" => Ok(DispatchOutcome::Result(
            vectors::drop_index(&req.params, ctx).await?,
        )),
        "corpus.rebuild" => Ok(DispatchOutcome::Result(
            corpus::rebuild(&req.params, ctx).await?,
        )),
        "stamps.reconcile" => Ok(DispatchOutcome::Result(
            stamps::reconcile(&req.params, ctx).await?,
        )),
        "remove" => Ok(DispatchOutcome::Result(
            remove::run(&req.params, ctx).await?,
        )),
        "papers.remove" => Ok(DispatchOutcome::Result(
            papers_remove::run(&req.params, ctx).await?,
        )),
        "papers.corpus_rebuild" => Ok(DispatchOutcome::Result(
            papers_corpus::rebuild(&req.params, ctx).await?,
        )),
        "papers.vectors_rebuild" => Ok(DispatchOutcome::Result(
            papers_vectors::rebuild(&req.params, ctx).await?,
        )),
        "papers.vectors_reembed" => Ok(DispatchOutcome::Result(
            papers_vectors::reembed(&req.params, ctx).await?,
        )),
        "papers.vectors_reset" => Ok(DispatchOutcome::Result(
            papers_vectors::reset(&req.params, ctx).await?,
        )),
        "papers.vectors_drop" => Ok(DispatchOutcome::Result(
            papers_vectors::drop_index(&req.params, ctx).await?,
        )),
        "papers.stamps_reconcile" => Ok(DispatchOutcome::Result(
            papers_stamps::reconcile(&req.params, ctx).await?,
        )),
        "papers.dryrun" => Ok(DispatchOutcome::Result(
            papers_dryrun::run(&req.params, ctx).await?,
        )),
        "papers.metadata.reaudit" => Ok(DispatchOutcome::Result(
            papers_metadata::reaudit(&req.params, ctx).await?,
        )),
        "papers.metadata.set" => Ok(DispatchOutcome::Result(
            papers_metadata::set(&req.params, ctx).await?,
        )),
        "papers.metadata.clear" => Ok(DispatchOutcome::Result(
            papers_metadata::clear(&req.params, ctx).await?,
        )),
        "papers.metadata.void" => Ok(DispatchOutcome::Result(
            papers_metadata::void(&req.params, ctx).await?,
        )),
        "papers.metadata.ack" => Ok(DispatchOutcome::Result(
            papers_metadata::ack(&req.params, ctx).await?,
        )),
        "papers.metadata.approve" => Ok(DispatchOutcome::Result(
            papers_metadata::approve(&req.params, ctx).await?,
        )),
        "papers.metadata.reject" => Ok(DispatchOutcome::Result(
            papers_metadata::reject(&req.params, ctx).await?,
        )),
        "papers.metadata.reopen" => Ok(DispatchOutcome::Result(
            papers_metadata::reopen(&req.params, ctx).await?,
        )),
        "papers.metadata.contributor_add" => Ok(DispatchOutcome::Result(
            papers_metadata::contributor_add(&req.params, ctx).await?,
        )),
        "papers.metadata.contributor_remove" => Ok(DispatchOutcome::Result(
            papers_metadata::contributor_remove(&req.params, ctx).await?,
        )),
        "dryrun" => Ok(DispatchOutcome::Result(
            dryrun::run(&req.params, ctx).await?,
        )),
        "verify.run" => Ok(DispatchOutcome::Result(verify::run(ctx).await?)),
        "library.fork" => Ok(DispatchOutcome::Result(
            libraries::fork(&req.params, ctx).await?,
        )),
        "library.set_default" => Ok(DispatchOutcome::Result(
            libraries::set_default(&req.params, ctx).await?,
        )),
        "diagnose.run" => Ok(DispatchOutcome::Result(
            diagnose::run(&req.params, ctx).await?,
        )),
        "daemon.methods" => Ok(DispatchOutcome::Result(meta::methods(ctx))),
        "daemon.mcp_tools" => Ok(DispatchOutcome::Result(meta::mcp_tools(ctx))),
        "tray.focus" => Ok(DispatchOutcome::Result(tray::focus(ctx))),
        other => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        )),
    }
}

/// JSON-RPC application code returned when the daemon cannot honour a
/// queue-bound write because the queue worker was not spawned.
/// Stable: callers (`bookrack-mcp` clients, the CLI) match on it to
/// distinguish a misconfigured headless entry from a transient busy
/// state.
pub const QUEUE_WORKER_DISABLED: i32 = -32002;

/// Method names that route work through the persistent queue worker.
/// In a headless `bookrack-mcp` profile without `--with-queue-worker`,
/// the dispatch short-circuits these to a `-32002 not_ready` response
/// rather than enqueueing tasks no one will run.
fn is_queue_bound_method(method: &str) -> bool {
    matches!(
        method,
        "ingest.submit"
            | "ingest.cancel"
            | "glean.submit"
            | "vectors.rebuild"
            | "vectors.reembed"
            | "vectors.reset"
            | "vectors.drop"
            | "corpus.rebuild"
            | "stamps.reconcile"
            | "remove"
            | "papers.remove"
            | "papers.corpus_rebuild"
            | "papers.vectors_rebuild"
            | "papers.vectors_reembed"
            | "papers.vectors_reset"
            | "papers.vectors_drop"
            | "papers.stamps_reconcile"
            | "papers.dryrun"
            | "papers.metadata.reaudit"
            | "papers.metadata.set"
            | "papers.metadata.clear"
            | "papers.metadata.void"
            | "papers.metadata.ack"
            | "papers.metadata.approve"
            | "papers.metadata.reject"
            | "papers.metadata.reopen"
            | "papers.metadata.contributor_add"
            | "papers.metadata.contributor_remove"
            | "metadata.advance"
            | "dryrun"
    )
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
}
