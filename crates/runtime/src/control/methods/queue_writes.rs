// SPDX-License-Identifier: Apache-2.0

//! `queue.pause` / `queue.resume` / `queue.clear` JSON-RPC handlers.
//!
//! Each method mutates either the worker-loop pause flag, the on-disk
//! queue document, or both, and emits a single
//! [`crate::control::events::Event::QueueTick`] so connected clients
//! observe the new state without polling. The handlers reuse the same
//! [`crate::queue::cancel_all_pending`] primitive that the in-process
//! REPL `queue clear` once called directly, keeping the trim semantics
//! in one place: only `Pending` rows are turned into `Cancelled`; rows
//! already `Running`, `Done`, `Failed`, or `Cancelled` are left alone.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bookrack_core::queue::QueueState;
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use ts_rs::TS;

use super::MethodContext;
use crate::control::events::{DegradedCause, Event, EventStreamHandle, QueueTick};
use crate::control::jsonrpc::{INTERNAL_ERROR, RpcError};
use crate::queue::{cancel_all_pending, derive_tick, save_atomic};

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct PauseResponse {
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct ClearResponse {
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
        if let Err(e) = save_atomic(&guard, &ctx.queue_state_path) {
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
    serde_json::to_value(ClearResponse { paused, cleared }).map_err(|e| {
        RpcError::new(
            INTERNAL_ERROR,
            format!("serialise queue.clear response: {e}"),
        )
    })
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
    serde_json::to_value(PauseResponse { paused }).map_err(|e| {
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
        if let Err(e) = save_atomic(&guard, queue_state_path) {
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
    if let Err(e) = save_atomic(&guard, queue_state_path) {
        guard.jobs = prev_jobs;
        return Err(RpcError::new(
            INTERNAL_ERROR,
            format!("persist queue state: {e:#}"),
        ));
    }
    let tick = derive_tick(&guard, None);
    Ok((out, tick))
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
}
