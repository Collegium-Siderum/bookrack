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
use std::sync::{Arc, Mutex};

use bookrack_config::{Config, LibrarySelection};
use bookrack_core::queue::QueueState;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::LibraryRegistry;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, broadcast};

use super::events::{DaemonState, Event, EventStreamHandle};
use super::jsonrpc::{BUSY, METHOD_NOT_FOUND, Request, RpcError};

pub mod corpus;
pub mod dryrun;
pub mod ingest;
pub mod metadata;
pub mod reads;
pub mod remove;
pub mod stamps;
pub mod vectors;

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
    match req.method.as_str() {
        "daemon.version" => Ok(DispatchOutcome::Result(reads::daemon_version(ctx))),
        "daemon.shutdown" => Ok(DispatchOutcome::Shutdown(reads::daemon_shutdown(ctx))),
        "status" => Ok(DispatchOutcome::Result(reads::status(ctx))),
        "doctor.gather" => Ok(DispatchOutcome::Result(reads::doctor_gather(ctx).await)),
        "queue.list" => Ok(DispatchOutcome::Result(reads::queue_list(
            &req.params,
            ctx,
        )?)),
        "library.list" => Ok(DispatchOutcome::Result(reads::library_list(ctx)?)),
        "library.info" => Ok(DispatchOutcome::Result(
            reads::library_info(&req.params, ctx).await?,
        )),
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
        "metadata.set" => Ok(DispatchOutcome::Result(
            metadata::set(&req.params, ctx).await?,
        )),
        "metadata.clear" => Ok(DispatchOutcome::Result(
            metadata::clear(&req.params, ctx).await?,
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
        "dryrun" => Ok(DispatchOutcome::Result(
            dryrun::run(&req.params, ctx).await?,
        )),
        other => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        )),
    }
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
