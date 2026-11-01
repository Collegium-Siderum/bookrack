// SPDX-License-Identifier: Apache-2.0

//! `queue.pause` / `queue.resume` / `queue.clear` / `queue.retry`
//! JSON-RPC handlers.
//!
//! Each method mutates either the worker-loop pause flag, the on-disk
//! queue document, or both, and emits a single
//! [`crate::control::events::Event::QueueTick`] so connected clients
//! observe the new state without polling. The handlers reuse the same
//! [`crate::queue::cancel_all_pending`] primitive that the in-process
//! REPL `queue clear` once called directly, keeping the trim semantics
//! in one place: only `Pending` rows are turned into `Cancelled`; rows
//! already `Running`, `Done`, `Failed`, or `Cancelled` are left alone.
//! `queue.retry` goes through [`mutate_jobs_and_persist`], the same
//! persist-or-roll-back path `ingest.cancel` takes.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bookrack_core::Problem;
use bookrack_core::queue::{JobState, QueueState};
use bookrack_ops::registry::RegistryError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::MethodContext;
use crate::control::error_map::{registry_err, rpc_from_problem};
use crate::control::events::{DegradedCause, Event, EventStreamHandle, QueueTick};
use crate::control::jsonrpc::{
    INTERNAL_ERROR, INVALID_LIBRARY, INVALID_PARAMS, JOB_NOT_FOUND, RpcError,
};
use crate::queue::{
    HeldJob, RetryRefusal, RetrySelector, cancel_all_pending, derive_tick, retry_terminal,
    save_atomic,
};

/// Reply to `queue.pause` / `queue.resume`. `ok` is the write-method
/// acknowledgement every control-plane write carries.
#[derive(Debug, Clone, Serialize)]
pub struct PauseResponse {
    pub ok: bool,
    pub paused: bool,
}

/// Reply to `queue.clear`: the pause flag as it stands after the trim
/// and the number of pending rows turned into `cancelled`.
#[derive(Debug, Clone, Serialize)]
pub struct ClearResponse {
    pub ok: bool,
    pub paused: bool,
    pub cleared: usize,
}

pub async fn pause(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    set_paused(ctx, true)
}

pub async fn resume(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    set_paused(ctx, false)
}

pub async fn clear(_params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let (cleared, tick) = {
        let mut guard = ctx.queue_state.lock().expect("queue state mutex poisoned");
        // Mirror `apply_pause`'s persist-then-publish ordering: snapshot
        // the jobs before mutating so a `save_atomic` failure rolls the
        // in-memory state back instead of letting a later successful
        // save flush the unintended cancellations to disk.
        let prev_jobs = guard.jobs.clone();
        let cleared = cancel_all_pending(&mut guard);
        if let Err(e) = save_atomic(&mut guard, &ctx.queue_state_path) {
            guard.jobs = prev_jobs;
            return Err(RpcError::new(
                INTERNAL_ERROR,
                format!("persist queue state: {e:#}"),
            ));
        }
        let tick = derive_tick(&guard, None);
        (cleared, tick)
    };
    ctx.event_stream.publish(Event::QueueTick(tick));
    let paused = ctx.queue_paused.load(Ordering::Acquire);
    serde_json::to_value(ClearResponse {
        ok: true,
        paused,
        cleared,
    })
    .map_err(|e| {
        RpcError::new(
            INTERNAL_ERROR,
            format!("serialise queue.clear response: {e}"),
        )
    })
}

/// Parameters of `queue.retry`: exactly one of the two selectors.
#[derive(Debug, Deserialize)]
pub struct QueueRetryParams {
    #[serde(default)]
    state: Option<JobState>,
    #[serde(default)]
    job_id: Option<String>,
}

/// Reply to `queue.retry`: the full ids flipped back to `pending`, the
/// selected rows left in place because their library is not mounted,
/// each in queue order, and the pause flag that decides whether the
/// worker picks the flipped rows up now or after `queue.resume`.
#[derive(Debug, Clone, Serialize)]
pub struct RetryResponse {
    pub ok: bool,
    pub retried: Vec<String>,
    pub held: Vec<HeldJob>,
    pub paused: bool,
}

/// Flip terminal rows back to `pending` in place — every row in one
/// retryable state, or the one row an id fragment resolves to. A
/// fragment that matches no row or several is `JOB_NOT_FOUND`; a selection that
/// is not exactly one of `state` / `job_id`, a state outside the
/// retryable set, or a fragment landing on a row in another state is
/// `INVALID_PARAMS`.
///
/// Each selected row's library is looked up in the registry under the
/// queue lock, the lock `library.unmount` counts and removes under. A
/// state selection holds the rows whose library is not mounted; a
/// fragment landing on such a row is `INVALID_LIBRARY`.
pub async fn retry(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: QueueRetryParams = match params {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::new(INVALID_PARAMS, format!("invalid queue.retry params: {e}"))
        })?,
        _ => return Err(RpcError::new(INVALID_PARAMS, "missing queue.retry params")),
    };
    let selector = match (parsed.state, parsed.job_id) {
        (Some(state), None) => RetrySelector::State(state),
        (None, Some(job_id)) => RetrySelector::IdFragment(job_id),
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "queue.retry takes exactly one of `state` or `job_id`",
            ));
        }
    };
    let (outcome, tick) =
        mutate_jobs_and_persist(&ctx.queue_state, &ctx.queue_state_path, |guard| {
            let mut not_mounted = None;
            let mut fault = None;
            let outcome = retry_terminal(guard, &selector, |library| {
                match ctx.registry.get(Some(library)) {
                    Ok(_) => true,
                    Err(
                        e @ (RegistryError::LibraryUnknown { .. }
                        | RegistryError::LibraryUnmounted { .. }),
                    ) => {
                        not_mounted = Some(e);
                        false
                    }
                    Err(e) => {
                        fault.get_or_insert(e);
                        false
                    }
                }
            });
            if let Some(e) = fault {
                return Err(registry_err(e));
            }
            outcome.map_err(|refusal| {
                let code = match &refusal {
                    RetryRefusal::LibraryNotMounted { id, library } => {
                        return rpc_from_problem(
                            INVALID_LIBRARY,
                            unmounted_retry(id, library, not_mounted.as_ref()),
                        );
                    }
                    RetryRefusal::NoMatch { .. } | RetryRefusal::Ambiguous { .. } => JOB_NOT_FOUND,
                    RetryRefusal::StateNotRetryable(_)
                    | RetryRefusal::EmptyFragment
                    | RetryRefusal::NotRetryable { .. } => INVALID_PARAMS,
                };
                RpcError::new(code, format!("queue.retry: {refusal}"))
            })
        })?;
    ctx.event_stream.publish(Event::QueueTick(tick));
    let paused = ctx.queue_paused.load(Ordering::Acquire);
    serde_json::to_value(RetryResponse {
        ok: true,
        retried: outcome.retried,
        held: outcome.held,
        paused,
    })
    .map_err(|e| {
        RpcError::new(
            INTERNAL_ERROR,
            format!("serialise queue.retry response: {e}"),
        )
    })
}

/// The refusal of a retry naming job `id`, whose row is against
/// `library`, which `cause` says the registry does not serve. The
/// summary names the job and the library; the detail is the bring-up
/// reason for a library bring-up skipped, and the mounted names for one
/// the registry does not carry. The hint names the mount and the retry
/// once each: a skip's own hint already closes with the mount, so only
/// the retry follows it.
fn unmounted_retry(id: &str, library: &str, cause: Option<&RegistryError>) -> Problem {
    let next =
        format!("`bookrack libraries mount {library}`, then `bookrack queue retry --job {id}`.");
    let problem = Problem::new(format!(
        "cannot retry job {id:?}: library {library:?} is not mounted"
    ));
    match cause {
        Some(RegistryError::LibraryUnmounted { reason, .. }) => {
            let detail = match &reason.data.detail {
                Some(detail) => format!("Bring-up skipped it: {}. {detail}", reason.summary),
                None => format!("Bring-up skipped it: {}.", reason.summary),
            };
            let hint = match &reason.data.hint {
                Some(hint) => {
                    format!("{hint} Once it is mounted, run `bookrack queue retry --job {id}`.")
                }
                None => format!("Run {next}"),
            };
            problem.detail(detail).hint(hint)
        }
        Some(RegistryError::LibraryUnknown { available, .. }) => {
            let mounted = if available.is_empty() {
                "none".to_owned()
            } else {
                available.join(", ")
            };
            problem
                .detail(format!("Mounted libraries: {mounted}."))
                .hint(format!("Run {next}"))
        }
        _ => problem.hint(format!("Run {next}")),
    }
}

fn set_paused(ctx: &MethodContext, paused: bool) -> Result<Value, RpcError> {
    let tick = apply_pause(
        paused,
        &ctx.queue_paused,
        &ctx.queue_state,
        &ctx.queue_state_path,
        &ctx.event_stream,
    )?;
    ctx.event_stream.publish(Event::QueueTick(tick));
    serde_json::to_value(PauseResponse { ok: true, paused }).map_err(|e| {
        RpcError::new(
            INTERNAL_ERROR,
            format!("serialise queue.pause response: {e}"),
        )
    })
}

/// Persist the `paused` flag to disk and only then flip the in-memory
/// `queue_paused` atomic. If `save_atomic` returns `Err`, the
/// in-memory `QueueState::paused` is restored to its previous value
/// and the atomic is left untouched, so the running worker and the
/// on-disk document never disagree across a restart.
///
/// A successful resume also clears the worker's failure-pause
/// degraded cause: the operator's `queue resume` is the
/// acknowledgement that ends the condition. Clearing an inactive
/// cause is a no-op, and an operator-initiated pause never sets one.
fn apply_pause(
    paused: bool,
    queue_paused: &AtomicBool,
    queue_state: &Mutex<QueueState>,
    queue_state_path: &Path,
    events: &EventStreamHandle,
) -> Result<QueueTick, RpcError> {
    let tick = {
        let mut guard = queue_state.lock().expect("queue state mutex poisoned");
        let prev = guard.paused;
        guard.paused = paused;
        if let Err(e) = save_atomic(&mut guard, queue_state_path) {
            guard.paused = prev;
            return Err(RpcError::new(
                INTERNAL_ERROR,
                format!("persist queue state: {e:#}"),
            ));
        }
        derive_tick(&guard, None)
    };
    queue_paused.store(paused, Ordering::Release);
    if !paused {
        events.set_degraded(DegradedCause::QueueFailurePause, false);
    }
    Ok(tick)
}

/// Apply `mutate` to the locked queue, persist the result, and restore
/// the jobs list if the persist fails — so a job the caller was told was
/// rejected never survives in memory for the worker to run. `mutate` may
/// itself fail (e.g. an ambiguous cancel prefix); that error is returned
/// after rolling back, with no persist attempted. On success returns
/// `mutate`'s value together with the fresh tick to publish.
pub(super) fn mutate_jobs_and_persist<T>(
    queue_state: &Mutex<QueueState>,
    queue_state_path: &Path,
    mutate: impl FnOnce(&mut QueueState) -> Result<T, RpcError>,
) -> Result<(T, QueueTick), RpcError> {
    let mut guard = queue_state
        .lock()
        .map_err(|_| RpcError::new(INTERNAL_ERROR, "queue state lock poisoned"))?;
    let prev_jobs = guard.jobs.clone();
    let out = match mutate(&mut guard) {
        Ok(out) => out,
        Err(e) => {
            guard.jobs = prev_jobs;
            return Err(e);
        }
    };
    if let Err(e) = save_atomic(&mut guard, queue_state_path) {
        guard.jobs = prev_jobs;
        return Err(RpcError::new(
            INTERNAL_ERROR,
            format!("persist queue state: {e:#}"),
        ));
    }
    let tick = derive_tick(&guard, None);
    Ok((out, tick))
}

/// Append rows against `library` through [`mutate_jobs_and_persist`],
/// confirming under the queue lock that the registry still serves
/// `library` before `enqueue` runs. `library.unmount` counts queued
/// work and removes a library under the same lock, so a library
/// removed after the caller resolved it is `INVALID_LIBRARY` here and
/// no row is written.
pub(super) fn enqueue_against<T>(
    ctx: &MethodContext,
    library: &str,
    enqueue: impl FnOnce(&mut QueueState) -> T,
) -> Result<(T, QueueTick), RpcError> {
    mutate_jobs_and_persist(&ctx.queue_state, &ctx.queue_state_path, |guard| {
        ctx.registry.get(Some(library)).map_err(registry_err)?;
        Ok(enqueue(guard))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_pause_flips_atomic_after_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.json");
        let queue_paused = AtomicBool::new(false);
        let queue_state = Mutex::new(QueueState::default());
        let events = EventStreamHandle::default();

        apply_pause(true, &queue_paused, &queue_state, &path, &events).expect("save succeeds");

        assert!(queue_paused.load(Ordering::Acquire));
        assert!(queue_state.lock().unwrap().paused);
        assert!(path.exists(), "save_atomic must have written the document");
    }

    #[test]
    fn resume_clears_the_failure_pause_degraded_cause() {
        use crate::control::events::{DaemonState, DaemonStateFlag};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.json");
        let queue_paused = AtomicBool::new(true);
        let queue_state = Mutex::new(QueueState {
            paused: true,
            ..QueueState::default()
        });
        let flag = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let events = EventStreamHandle::new(8, Arc::clone(&flag));
        events.set_degraded(DegradedCause::QueueFailurePause, true);
        assert_eq!(flag.load(), DaemonState::Degraded);

        apply_pause(false, &queue_paused, &queue_state, &path, &events).expect("save succeeds");

        assert!(!queue_paused.load(Ordering::Acquire));
        assert_eq!(
            flag.load(),
            DaemonState::Idle,
            "resume must clear the failure-pause cause"
        );
    }

    #[test]
    fn operator_pause_does_not_touch_degraded() {
        use crate::control::events::{DaemonState, DaemonStateFlag};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.json");
        let queue_paused = AtomicBool::new(false);
        let queue_state = Mutex::new(QueueState::default());
        let flag = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let events = EventStreamHandle::new(8, Arc::clone(&flag));

        apply_pause(true, &queue_paused, &queue_state, &path, &events).expect("save succeeds");

        assert_eq!(
            flag.load(),
            DaemonState::Idle,
            "an operator-initiated pause is not a degraded condition"
        );
    }

    #[test]
    fn apply_pause_leaves_atomic_untouched_when_save_fails() {
        // Build a destination whose parent is a regular file: any
        // `create_dir_all` against it errors out, which surfaces as a
        // `save_atomic` failure on every platform.
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"").expect("seed blocker file");
        let path = blocker.join("queue.json");

        let queue_paused = AtomicBool::new(false);
        let queue_state = Mutex::new(QueueState::default());

        let err = apply_pause(
            true,
            &queue_paused,
            &queue_state,
            &path,
            &EventStreamHandle::default(),
        )
        .expect_err("save_atomic must fail under a file-shaped parent");
        assert_eq!(err.code, INTERNAL_ERROR);
        assert!(
            !queue_paused.load(Ordering::Acquire),
            "atomic must not flip when persist fails"
        );
        assert!(
            !queue_state.lock().unwrap().paused,
            "in-memory QueueState::paused must be restored to its prior value"
        );
    }

    fn enqueue_one(guard: &mut QueueState) -> Vec<String> {
        crate::queue::enqueue_files(
            guard,
            &[std::path::PathBuf::from("book.epub")],
            "lib",
            bookrack_core::ItemKind::Book,
            crate::queue::Priority::Normal,
            false,
            false,
            None,
        )
    }

    #[test]
    fn mutate_jobs_and_persist_persists_and_returns_tick_on_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.json");
        let queue_state = Mutex::new(QueueState::default());

        let (ids, tick) =
            mutate_jobs_and_persist(&queue_state, &path, |g| Ok::<_, RpcError>(enqueue_one(g)))
                .expect("persist succeeds");

        assert_eq!(ids.len(), 1);
        assert_eq!(tick.pending, 1);
        assert_eq!(queue_state.lock().unwrap().jobs.len(), 1);
        assert!(path.exists(), "save_atomic must have written the document");
    }

    #[test]
    fn mutate_jobs_and_persist_rolls_back_jobs_when_save_fails() {
        // Parent is a regular file, so save_atomic fails on every platform.
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"").expect("seed blocker file");
        let path = blocker.join("queue.json");
        let queue_state = Mutex::new(QueueState::default());

        let err =
            mutate_jobs_and_persist(&queue_state, &path, |g| Ok::<_, RpcError>(enqueue_one(g)))
                .expect_err("save_atomic must fail under a file-shaped parent");

        assert_eq!(err.code, INTERNAL_ERROR);
        assert!(
            queue_state.lock().unwrap().jobs.is_empty(),
            "the enqueued job must be rolled back so the worker never runs it"
        );
    }

    #[test]
    fn mutate_jobs_and_persist_returns_mutation_error_without_persisting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.json");
        let queue_state = Mutex::new(QueueState::default());

        let err = mutate_jobs_and_persist(&queue_state, &path, |g| {
            enqueue_one(g);
            Err::<(), _>(RpcError::new(INTERNAL_ERROR, "mutation refused"))
        })
        .expect_err("mutation error propagates");

        assert_eq!(err.code, INTERNAL_ERROR);
        assert!(
            queue_state.lock().unwrap().jobs.is_empty(),
            "a mutation that returns an error must be rolled back in full"
        );
        assert!(
            !path.exists(),
            "a failed mutation must not persist the queue document"
        );
    }

    /// Append one pending book row against `library` and return its id.
    fn queue_one(state: &mut QueueState, library: &str) -> String {
        crate::queue::enqueue_files(
            state,
            &[std::path::PathBuf::from(format!("{library}.epub"))],
            library,
            bookrack_core::ItemKind::Book,
            crate::queue::Priority::Normal,
            false,
            false,
            None,
        )
        .remove(0)
    }

    /// A context serving `alpha`, with `beta` recorded as skipped at
    /// bring-up and `gamma` mounted and then unmounted at runtime, and
    /// one failed row against each, in that order. `beta`'s row is
    /// failed the way bring-up fails it, with the mount step in its
    /// message and the skip reason as its problem.
    fn mixed_queue() -> (MethodContext, [String; 3], [tempfile::TempDir; 2]) {
        use bookrack_ops::registry::UnmountedLibrary;

        use super::super::{test_library_handle, test_method_context};
        use crate::queue::{JobFailure, fail_jobs_against};

        let dirs = [
            tempfile::tempdir().expect("tempdir"),
            tempfile::tempdir().expect("tempdir"),
        ];
        let ctx = test_method_context(dirs[0].path(), Some("alpha"));
        let beta = UnmountedLibrary::new(
            "beta",
            "/disk/beta",
            Problem::new("the catalog schema is newer than this build")
                .hint("Upgrade bookrack. Then run `bookrack libraries mount beta`."),
        );
        ctx.registry
            .record_unmounted(beta.clone())
            .expect("record beta as skipped");
        ctx.registry
            .mount(test_library_handle(dirs[1].path(), "gamma"))
            .expect("gamma mounts");
        ctx.registry.unmount("gamma").expect("gamma unmounts");

        let ids = {
            let mut state = ctx.queue_state.lock().expect("queue lock");
            let ids = ["alpha", "beta", "gamma"].map(|library| queue_one(&mut state, library));
            let earlier = JobFailure::text("earlier failure");
            fail_jobs_against(&mut state, "alpha", &earlier);
            fail_jobs_against(
                &mut state,
                "beta",
                &JobFailure {
                    message: crate::unmounted::queue_failure_message(&beta),
                    problem: Some(beta.reason().clone()),
                },
            );
            fail_jobs_against(&mut state, "gamma", &earlier);
            ids
        };
        (ctx, ids, dirs)
    }

    /// A state retry flips the rows whose library is served and leaves
    /// every other selected row exactly as it was, a library skipped at
    /// bring-up and one unmounted at runtime alike, naming them in
    /// `held` in queue order.
    #[tokio::test]
    async fn a_state_retry_holds_rows_against_a_library_that_is_not_mounted() {
        let (ctx, [alpha, beta, gamma], _dirs) = mixed_queue();
        let before = ctx.queue_state.lock().unwrap().jobs.clone();

        let reply = retry(&Some(serde_json::json!({ "state": "failed" })), &ctx)
            .await
            .expect("a state retry succeeds");

        assert_eq!(reply["retried"], serde_json::json!([alpha]), "{reply}");
        assert_eq!(
            reply["held"],
            serde_json::json!([
                { "id": beta, "library": "beta" },
                { "id": gamma, "library": "gamma" },
            ]),
            "{reply}"
        );
        let jobs = ctx.queue_state.lock().unwrap().jobs.clone();
        assert_eq!(jobs[0].state, JobState::Pending);
        assert_eq!(
            jobs[1..],
            before[1..],
            "a held row keeps its state, its error, and its problem"
        );
    }

    /// A `job_id` retry of a row whose library is not mounted is refused
    /// with `INVALID_LIBRARY` and changes nothing. The refusal names the
    /// job and the library rather than calling the library unknown,
    /// carries the bring-up reason when there is one, and its hint names
    /// the mount once and the retry once: after the record's own hint,
    /// which already closes with the mount, only the retry is added.
    #[tokio::test]
    async fn a_job_retry_against_a_library_that_is_not_mounted_is_refused() {
        use crate::control::jsonrpc::INVALID_LIBRARY;

        let (ctx, [_, beta, gamma], _dirs) = mixed_queue();
        let before = ctx.queue_state.lock().unwrap().jobs.clone();

        let mut refusals = Vec::new();
        for (id, library) in [(&beta, "beta"), (&gamma, "gamma")] {
            let err = retry(&Some(serde_json::json!({ "job_id": id })), &ctx)
                .await
                .expect_err("a row against an unmounted library is not retried");
            assert_eq!(err.code, INVALID_LIBRARY, "{err:?}");
            assert!(err.message.contains(id.as_str()), "{err:?}");
            assert!(
                err.message
                    .contains(&format!("library {library:?} is not mounted")),
                "{err:?}"
            );
            let data = err.data.clone().unwrap_or_default();
            let hint = data["hint"].as_str().unwrap_or_default().to_owned();
            assert_eq!(
                hint.matches(&format!("`bookrack libraries mount {library}`"))
                    .count(),
                1,
                "{err:?}"
            );
            assert_eq!(
                hint.matches(&format!("`bookrack queue retry --job {id}`"))
                    .count(),
                1,
                "{err:?}"
            );
            refusals.push((data, hint));
        }
        let (beta_data, beta_hint) = &refusals[0];
        assert!(
            beta_data["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("the catalog schema is newer than this build"),
            "{beta_data}"
        );
        assert_eq!(
            beta_hint,
            &format!(
                "Upgrade bookrack. Then run `bookrack libraries mount beta`. Once it is \
                 mounted, run `bookrack queue retry --job {beta}`."
            ),
            "the record's own hint, closed with the mount, gains the retry alone"
        );
        let (_, gamma_hint) = &refusals[1];
        assert_eq!(
            gamma_hint,
            &format!(
                "Run `bookrack libraries mount gamma`, then `bookrack queue retry --job {gamma}`."
            ),
        );
        let (gamma_data, _) = &refusals[1];
        assert!(
            !gamma_data.to_string().contains("no library named"),
            "{gamma_data}"
        );
        assert_eq!(
            ctx.queue_state.lock().unwrap().jobs,
            before,
            "a refused retry changes nothing"
        );
    }
}
