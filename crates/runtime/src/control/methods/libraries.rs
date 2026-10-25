// SPDX-License-Identifier: Apache-2.0

//! `library.fork` — clone the active library into a sibling data
//! root and register it in the user's library registry. The MCP
//! parity work in Phase 5 reuses this same handler.
//!
//! `library.set_default` — re-point the registry's default-library
//! pointer at one of its known libraries. The change is persisted to
//! the on-disk registry, then the daemon's in-memory pointer — a cache
//! of that on-disk value — is refreshed, so the default survives a
//! daemon restart and the running daemon's routing follows immediately.

use std::path::PathBuf;

use bookrack_config::{registry_target_path, set_default_library};
use serde::Deserialize;
use serde_json::{Value, json};

use super::super::error_map::{config_err, handler_err, mount_err, registry_err};
use super::super::events::Event;
use super::super::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, NOT_READY, RpcError};
use super::MethodContext;
use super::run_write;
use crate::cmd::libraries::CopyMode;

#[derive(Debug, Deserialize)]
pub struct LibraryForkParams {
    pub new_name: String,
    pub data_dir: PathBuf,
    /// `"hardlink"` (default) or `"copy"`. Mirrors the cli's
    /// `--copy-mode` flag.
    #[serde(default = "default_copy_mode")]
    pub copy_mode: String,
    /// Must be `true`; the control-plane runner does not prompt for
    /// confirmation. The cli client holds any interactive prompt and
    /// forwards `yes: true` once the operator confirms.
    #[serde(default)]
    pub yes: bool,
    /// The library being forked — the source of the clone. Absent
    /// means the registry's current default. `library.fork` is the one
    /// method that legitimately holds two libraries at once, so the
    /// source is named explicitly rather than inherited from whichever
    /// library the daemon came up under.
    #[serde(default)]
    library: Option<String>,
}

fn default_copy_mode() -> String {
    "hardlink".to_string()
}

pub async fn fork(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let raw = params
        .clone()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "library.fork: missing params"))?;
    let parsed: LibraryForkParams = serde_json::from_value(raw)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("library.fork params: {e}")))?;
    if !parsed.yes {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "library.fork requires yes=true; the client is responsible for any operator prompt",
        ));
    }
    let mode = match parsed.copy_mode.as_str() {
        "hardlink" => CopyMode::Hardlink,
        "copy" => CopyMode::Copy,
        other => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!("library.fork copy_mode: expected hardlink or copy, got {other:?}"),
            ));
        }
    };
    let registry_path = registry_target_path().ok_or_else(|| {
        RpcError::new(
            crate::control::jsonrpc::INTERNAL_ERROR,
            "library.fork: no registry location: set BOOKRACK_REGISTRY=<path> or ensure the \
             platform config directory is available",
        )
    })?;
    let handle = ctx
        .registry
        .get(parsed.library.as_deref())
        .map_err(registry_err)?;
    let cfg = handle.cfg_arc();
    let target = parsed.data_dir.clone();
    let new_name = parsed.new_name.clone();
    // A daemon that cannot mount still forks: the clone is a real
    // library on disk either way, and refusing to make one because
    // this process cannot serve it would be the wrong trade.
    let mounter = ctx.mounter.clone();
    run_write(ctx, handle.name(), move || async move {
        crate::cmd::libraries::fork(&cfg, &new_name, &target, &registry_path, mode, true, |_| {
            Ok(true)
        })
        .map_err(|e| handler_err("library.fork", e))?;
        // Mount failure does not roll the fork back: the library is
        // built and registered by now, and undoing it would delete
        // data to report a serving problem. The fork succeeds and says
        // it is not being served, which leaves the operator one
        // `libraries mount` away instead of one restore.
        let mounted = match mounter {
            Some(mounter) => match mounter.mount(&new_name).await {
                Ok(()) => Ok(()),
                Err(e) => Err(format!("{e:#}")),
            },
            None => Err(
                "this daemon dispatches without library mounts, so it cannot serve the clone"
                    .to_string(),
            ),
        };
        Ok(json!({
            "new_name": new_name,
            "data_dir": target,
            "mounted": mounted.is_ok(),
            "mount_error": mounted.err(),
        }))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct LibrarySetDefaultParams {
    pub name: String,
}

/// Re-point the default-library pointer at `name`, persisting it to the
/// registry.
///
/// The registry file is the single home of the default. The name is
/// validated against the daemon's registered libraries first, so an
/// unknown name is rejected before any write; the change is then written
/// to the on-disk registry, and only afterwards is the daemon's
/// in-memory pointer — a cache of the on-disk value — refreshed. Writing
/// disk before memory keeps the truth ahead of its cache: a memory flip
/// that outran a failed disk write would silently evaporate on restart.
/// Fires a `library.changed` event so subscribers refresh their view.
pub async fn set_default(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let raw = params
        .clone()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "library.set_default: missing params"))?;
    let parsed: LibrarySetDefaultParams = serde_json::from_value(raw)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("library.set_default params: {e}")))?;

    // Validate before touching disk, so an unknown name fails without a
    // write. The mounted set is not the whole answer: a library the
    // registry carries but this daemon is not serving is a legitimate
    // target, and refusing it would mean the pointer could only ever
    // move between the libraries that happened to be mounted. Mount it
    // instead, so the daemon is serving whatever it is about to route
    // unnamed calls to.
    if ctx.registry.get(Some(&parsed.name)).is_err() {
        let mounter = require_mounter("library.set_default", ctx)?;
        mounter.mount(&parsed.name).await.map_err(mount_err)?;
    }

    // Persist to the registry, then refresh the in-memory cache.
    let registry_path = registry_target_path().ok_or_else(|| {
        RpcError::new(
            INTERNAL_ERROR,
            "library.set_default: no registry file to persist the default",
        )
    })?;
    set_default_library(&registry_path, &parsed.name).map_err(config_err)?;
    ctx.registry
        .set_default(&parsed.name)
        .map_err(registry_err)?;

    ctx.event_stream.publish(Event::LibraryChanged {
        library: parsed.name.clone(),
    });
    Ok(json!({ "ok": true, "name": parsed.name }))
}

#[derive(Debug, Deserialize)]
pub struct LibraryMountParams {
    pub name: String,
}

/// Open a registered library and add it to the set this daemon serves.
///
/// The name is a registry name, never a path: the daemon resolves it
/// through the registry, so what it mounts is what the registry
/// declares, and registering a new root stays a separate act with its
/// own failure modes.
///
/// Runs through [`run_write`] like every other change to what the
/// daemon serves: the write mutex keeps a mount from racing another
/// write, MCP is paused for its duration, and the `library.changed`
/// event a subscriber needs to refresh its view of the library set is
/// published on success by the wrapper itself.
pub async fn mount(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let name = mount_target("library.mount", params)?;
    let mounter = require_mounter("library.mount", ctx)?;
    let target = name.clone();
    run_write(ctx, &name, move || async move {
        mounter.mount(&target).await.map_err(mount_err)?;
        tracing::info!(library = %target, "library mounted at runtime");
        Ok(json!({ "ok": true, "name": target }))
    })
    .await
}

/// Stop serving a library and let go of its data root.
///
/// Refuses the library the daemon came up under, the last mounted
/// library, and a library with queued work; see
/// [`crate::mount::Mounter::unmount`] for why each of the three would
/// otherwise leave the daemon describing something it no longer serves.
///
/// The root lock is released when the last caller holding the library's
/// handle is done with it, which may be after this call returns: an
/// in-flight read keeps the root held until it finishes.
pub async fn unmount(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let name = mount_target("library.unmount", params)?;
    unmount_library(ctx, name, || {}).await
}

/// The body of [`unmount`]. `before_remove` runs once the queued work
/// against `name` has been counted and before the library leaves the
/// registry.
async fn unmount_library(
    ctx: &MethodContext,
    name: String,
    before_remove: impl FnOnce() + Send + 'static,
) -> Result<Value, RpcError> {
    let mounter = require_mounter("library.unmount", ctx)?;
    // The in-memory key, so a root selected by path is protected under
    // the name the registry serves it as.
    let primary = Some(ctx.library_name.clone());
    let queue_state = std::sync::Arc::clone(&ctx.queue_state);
    let target = name.clone();
    run_write(ctx, &name, move || async move {
        // The queued work is counted and the library removed under one
        // hold of the queue lock. A submit writes its row under the
        // same lock after confirming its library is mounted, so the row
        // lands either before the count, which then refuses this
        // unmount, or after the removal, which refuses the submit.
        let handle = {
            let queue = queue_state.lock().map_err(|_| {
                RpcError::new(
                    INTERNAL_ERROR,
                    "library.unmount: the queue state lock is poisoned",
                )
            })?;
            let facts = crate::mount::UnmountFacts {
                primary: primary.as_deref(),
                queued_jobs: queued_against(&queue, &target),
            };
            before_remove();
            mounter.unmount(&target, facts).map_err(mount_err)?
        };
        // Outside the queue lock: the last handle going may close the
        // library's stores.
        drop(handle);
        tracing::info!(library = %target, "library unmounted at runtime");
        Ok(json!({ "ok": true, "name": target }))
    })
    .await
}

/// Count the jobs against `library` the worker has not finished with.
fn queued_against(queue: &bookrack_core::queue::QueueState, library: &str) -> usize {
    queue
        .jobs
        .iter()
        .filter(|job| {
            job.library == library
                && matches!(
                    job.state,
                    bookrack_core::queue::JobState::Pending
                        | bookrack_core::queue::JobState::Running
                )
        })
        .count()
}

fn mount_target(method: &str, params: &Option<Value>) -> Result<String, RpcError> {
    let raw = params
        .clone()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("{method}: missing params")))?;
    let parsed: LibraryMountParams = serde_json::from_value(raw)
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("{method} params: {e}")))?;
    Ok(parsed.name)
}

/// The mount capability, or the refusal an entry point without one
/// owes its caller. Same shape as a queue-bound method reaching a
/// daemon that spawned no worker: the method exists, this process
/// cannot serve it.
fn require_mounter(
    method: &str,
    ctx: &MethodContext,
) -> Result<std::sync::Arc<crate::mount::Mounter>, RpcError> {
    ctx.mounter.clone().ok_or_else(|| {
        RpcError::new(
            NOT_READY,
            format!("{method} is not available in this entry point: it dispatches without a daemon bring-up, so it holds no library mounts"),
        )
    })
}

routed_params!(LibraryForkParams);

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use bookrack_core::ItemKind;
    use bookrack_core::queue::{JobState, QueueState};
    use bookrack_ops::Caller;

    use super::super::queue_writes::enqueue_against;
    use super::super::{test_library_handle, test_method_context};
    use super::*;
    use crate::control::jsonrpc::INVALID_LIBRARY;
    use crate::mount::Mounter;
    use crate::queue::Priority;

    /// How long a racing call is given to land when nothing holds it
    /// off. Only an implementation without the hold-off depends on it;
    /// with the queue lock held across both steps the outcome is the
    /// same however long the window is.
    const RACE_WINDOW: Duration = Duration::from_millis(200);

    /// `alpha`, the library the daemon came up under, and `beta`, both
    /// mounted, with a mounter bound to the same registry.
    fn two_library_context(alpha: &Path, beta: &Path) -> MethodContext {
        let mut ctx = test_method_context(alpha, Some("alpha"));
        ctx.registry
            .mount(test_library_handle(beta, "beta"))
            .expect("beta mounts beside alpha");
        ctx.mounter = Some(Arc::new(Mounter::new(
            Arc::clone(&ctx.registry),
            None,
            Caller::cli(),
        )));
        ctx
    }

    fn enqueue_one(guard: &mut QueueState, library: &str) -> Vec<String> {
        crate::queue::enqueue_files(
            guard,
            &[PathBuf::from("book.epub")],
            library,
            ItemKind::Book,
            Priority::Normal,
            false,
            false,
            None,
        )
    }

    /// Submit one book job against `library` on its own thread, the way
    /// the three submit handlers write their rows.
    fn submit_on_thread(
        ctx: &MethodContext,
        library: &'static str,
    ) -> thread::JoinHandle<Result<(), RpcError>> {
        let ctx = ctx.clone();
        thread::spawn(move || {
            enqueue_against(&ctx, library, |guard| enqueue_one(guard, library)).map(|_| ())
        })
    }

    /// Rows the worker has yet to finish whose library the registry no
    /// longer serves: each would fail on its next pull.
    fn stranded_rows(ctx: &MethodContext) -> Vec<String> {
        let state = ctx.queue_state.lock().expect("queue lock");
        state
            .jobs
            .iter()
            .filter(|job| matches!(job.state, JobState::Pending | JobState::Running))
            .filter(|job| ctx.registry.get(Some(&job.library)).is_err())
            .map(|job| format!("{} against {:?}", job.id, job.library))
            .collect()
    }

    /// A submit resolves its library before it takes the queue lock.
    /// The library is confirmed again under that lock, so one unmounted
    /// in between gets no row.
    #[test]
    fn a_submit_whose_library_was_unmounted_after_resolution_writes_no_row() {
        let (alpha, beta) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let ctx = two_library_context(alpha.path(), beta.path());

        let held = ctx.queue_state.lock().expect("queue lock");
        let submit = submit_on_thread(&ctx, "beta");
        thread::sleep(RACE_WINDOW);
        ctx.registry.unmount("beta").expect("beta is mounted");
        drop(held);

        let err = submit.join().expect("submit thread").expect_err(
            "a submit whose library left the registry before its row was written must be refused",
        );
        assert_eq!(err.code, INVALID_LIBRARY, "{err:?}");
        assert!(
            ctx.queue_state.lock().unwrap().jobs.is_empty(),
            "a refused submit leaves no row"
        );
    }

    /// An unmount that counted no queued work removes the library
    /// before a submit arriving in between can write: the submit waits,
    /// then finds the library gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_submit_racing_an_unmount_that_counted_first_is_refused() {
        let (alpha, beta) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let ctx = two_library_context(alpha.path(), beta.path());

        let (tx, rx) = mpsc::channel();
        let racer = ctx.clone();
        let reply = unmount_library(&ctx, "beta".to_string(), move || {
            let submit = submit_on_thread(&racer, "beta");
            thread::sleep(RACE_WINDOW);
            tx.send(submit).expect("hand the submit back");
        })
        .await;
        let submit = rx
            .recv()
            .expect("before_remove ran")
            .join()
            .expect("submit thread");

        reply.expect("beta had no queued work when the unmount counted");
        assert_eq!(
            stranded_rows(&ctx),
            Vec::<String>::new(),
            "a row written between the unmount's count and its removal is left against a library no longer served"
        );
        let err = submit.expect_err("the submit behind the unmount is refused");
        assert_eq!(err.code, INVALID_LIBRARY, "{err:?}");
    }

    /// A submit that took the queue lock first writes its row; the
    /// unmount behind it counts that row and refuses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unmount_racing_a_submit_that_wrote_first_is_refused() {
        let (alpha, beta) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let ctx = two_library_context(alpha.path(), beta.path());

        let (inside_tx, inside_rx) = mpsc::channel();
        let racer = ctx.clone();
        let submit = thread::spawn(move || {
            enqueue_against(&racer, "beta", |guard| {
                inside_tx.send(()).expect("signal the lock is held");
                thread::sleep(RACE_WINDOW);
                enqueue_one(guard, "beta")
            })
            .map(|_| ())
        });
        inside_rx.recv().expect("the submit holds the queue lock");

        let reply = unmount_library(&ctx, "beta".to_string(), || {}).await;
        submit
            .join()
            .expect("submit thread")
            .expect("the submit that took the lock first writes its row");

        let err = reply.expect_err("an unmount behind a written row is refused");
        assert_eq!(err.code, INVALID_PARAMS, "{err:?}");
        assert!(
            ctx.registry.get(Some("beta")).is_ok(),
            "a refused unmount leaves beta mounted"
        );
        assert_eq!(ctx.queue_state.lock().unwrap().jobs.len(), 1);
    }
}
