// SPDX-License-Identifier: Apache-2.0

//! Daemon-wide broadcast channel of control-plane events.
//!
//! Mirrors the shape [`bookrack_obs::stream::LogStreamHandle`] uses for
//! tracing events: a `tokio::sync::broadcast::Sender<Event>` shared
//! across the runtime, with one receiver per connected control-plane
//! client. The two handles are deliberately not unified — log events
//! and control-plane events have different shapes, lifetimes, and
//! consumers — but the operational model is identical.
//!
//! The `daemon.state` channel carries a single lifecycle value derived
//! from independent activity sources (RPC write session, queue jobs,
//! degraded conditions, shutdown) — see [`EventStreamHandle`] for the
//! source setters and the precedence that folds them into one
//! [`DaemonState`]. The remaining channels (`queue.tick`,
//! `worker.progress`, `library.changed`, `mcp.availability`, `log`)
//! each carry their own payload; the [`Event`] enum is the single
//! extension point.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bookrack_core::ItemKind;
use bookrack_core::queue::JobState;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast;
#[cfg(test)]
use ts_rs::TS;

/// Default capacity for the event broadcast. Matches the `obs`
/// log-event channel.
pub const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 512;

/// Discrete daemon lifecycle state, exposed to clients through both
/// the `status` method's `state` field and the `daemon.state` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
#[serde(rename_all = "snake_case")]
pub enum DaemonState {
    Idle,
    Writing,
    Degraded,
    Stopping,
    Working,
}

impl DaemonState {
    /// Encode the variant as the discriminant the [`DaemonStateFlag`]
    /// atomically stores.
    pub fn as_u8(self) -> u8 {
        match self {
            DaemonState::Idle => 0,
            DaemonState::Writing => 1,
            DaemonState::Degraded => 2,
            DaemonState::Stopping => 3,
            DaemonState::Working => 4,
        }
    }

    /// Inverse of [`DaemonState::as_u8`]. Unknown discriminants fall
    /// back to [`DaemonState::Idle`] so a corrupted atomic does not
    /// crash the dispatcher.
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => DaemonState::Writing,
            2 => DaemonState::Degraded,
            3 => DaemonState::Stopping,
            4 => DaemonState::Working,
            _ => DaemonState::Idle,
        }
    }
}

/// Shared atomic backing the `status.state` field. The dispatcher
/// reads it; the runtime writes through it from the bring-up and
/// shutdown coordinator.
#[derive(Debug)]
pub struct DaemonStateFlag(AtomicU8);

impl DaemonStateFlag {
    pub fn new(initial: DaemonState) -> Self {
        Self(AtomicU8::new(initial.as_u8()))
    }

    pub fn load(&self) -> DaemonState {
        DaemonState::from_u8(self.0.load(Ordering::SeqCst))
    }

    pub fn store(&self, state: DaemonState) {
        self.0.store(state.as_u8(), Ordering::SeqCst);
    }
}

impl Default for DaemonStateFlag {
    fn default() -> Self {
        Self::new(DaemonState::Idle)
    }
}

/// Ingest pipeline stage tag carried on [`Event::WorkerProgress`].
/// Aligned with the three top-level phases the ingest pipeline drives a
/// book through.
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Extract,
    Ingest,
    Embed,
}

/// Per-job snapshot of a terminal queue transition, attached to
/// [`QueueTick::last_finished`] so subscribers can render the most
/// recent outcome without re-fetching the queue document.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct JobOutcomeSummary {
    pub job_id: String,
    /// Which pipeline produced the finished job: `"book"` for ingest,
    /// `"paper"` for glean. Mirrored from the matching `QueueJob.kind`.
    #[cfg_attr(test, ts(type = "\"book\" | \"paper\""))]
    pub kind: ItemKind,
    #[cfg_attr(
        test,
        ts(
            type = "\"pending\" | \"running\" | \"done\" | \"skipped_duplicate\" | \"needs_ocr\" | \"failed\" | \"cancelled\""
        )
    )]
    pub state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[cfg_attr(test, ts(type = "string"))]
    pub finished_at: DateTime<Utc>,
}

/// Coarse view of the queue at one persisted moment. Each tick
/// follows a `save_atomic` on the queue snapshot, so the values
/// here are guaranteed to be derivable from the on-disk document.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct QueueTick {
    /// Id of the job currently in `Running`, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    pub pending: u32,
    pub running: u32,
    /// Outcome captured at the tick that closed out a job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_finished: Option<JobOutcomeSummary>,
}

/// Intra-stage progress emitted by the queue worker around each phase
/// boundary. `stage_progress` is a 0.0..=1.0 fraction when measurable,
/// otherwise omitted.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct WorkerProgress {
    pub job_id: String,
    pub stage: Stage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_progress: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Wire-level event published on the broadcast.
///
/// The enum tags itself with `channel` and carries the payload in
/// `value`, so dispatchers can serialise the variant directly into an
/// `event` notification.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
#[serde(tag = "channel", content = "value")]
pub enum Event {
    #[serde(rename = "daemon.state")]
    #[cfg_attr(test, ts(rename = "daemon.state"))]
    DaemonState(DaemonState),
    #[serde(rename = "queue.tick")]
    #[cfg_attr(test, ts(rename = "queue.tick"))]
    QueueTick(QueueTick),
    #[serde(rename = "worker.progress")]
    #[cfg_attr(test, ts(rename = "worker.progress"))]
    WorkerProgress(WorkerProgress),
    #[serde(rename = "library.changed")]
    #[cfg_attr(test, ts(rename = "library.changed"))]
    LibraryChanged { library: String },
    #[serde(rename = "mcp.availability")]
    #[cfg_attr(test, ts(rename = "mcp.availability"))]
    McpAvailability { paused: bool },
    #[serde(rename = "log")]
    #[cfg_attr(test, ts(rename = "log"))]
    Log(#[cfg_attr(test, ts(type = "Record<string, unknown>"))] bookrack_obs::stream::LogEvent),
}

impl Event {
    /// Name of the channel this event belongs to. Mirrors the
    /// `serde(rename)` tags above so client code can match without
    /// going through serialisation.
    pub fn channel(&self) -> &'static str {
        match self {
            Event::DaemonState(_) => "daemon.state",
            Event::QueueTick(_) => "queue.tick",
            Event::WorkerProgress(_) => "worker.progress",
            Event::LibraryChanged { .. } => "library.changed",
            Event::McpAvailability { .. } => "mcp.availability",
            Event::Log(_) => "log",
        }
    }

    /// Payload as a [`serde_json::Value`], suitable for the
    /// notification's `value` field.
    pub fn value(&self) -> serde_json::Value {
        match self {
            Event::DaemonState(state) => serde_json::to_value(state).unwrap_or_default(),
            Event::QueueTick(tick) => serde_json::to_value(tick).unwrap_or_default(),
            Event::WorkerProgress(progress) => serde_json::to_value(progress).unwrap_or_default(),
            Event::LibraryChanged { library } => serde_json::json!({ "library": library }),
            Event::McpAvailability { paused } => serde_json::json!({ "paused": paused }),
            Event::Log(event) => serde_json::to_value(event).unwrap_or_default(),
        }
    }
}

/// A persistent condition that holds the daemon in
/// [`DaemonState::Degraded`] while no activity outranks it. Each cause
/// is set and cleared independently; the daemon leaves `degraded` only
/// when every cause is clear.
#[derive(Debug, Clone, Copy)]
pub enum DegradedCause {
    /// The queue worker paused itself after a process-level job
    /// failure (resource exhaustion rather than a bad input).
    QueueFailurePause,
    /// The supervised reranker backend is crash-looping: repeated
    /// respawn attempts within one outage without reaching ready.
    RerankCrashloop,
}

impl DegradedCause {
    fn bit(self) -> u8 {
        match self {
            DegradedCause::QueueFailurePause => 1,
            DegradedCause::RerankCrashloop => 2,
        }
    }
}

/// Independent inputs the daemon lifecycle state derives from. Each
/// source is owned by exactly one producer; the resolver folds them
/// into a single [`DaemonState`] by precedence.
#[derive(Debug, Default)]
struct StateSources {
    /// Shutdown has been signalled; terminal and never cleared.
    stopping: bool,
    /// An RPC write session holds the runtime-wide write mutex.
    rpc_write: bool,
    /// Number of queue jobs currently executing.
    working_jobs: u32,
    /// Bitset of active [`DegradedCause`]s.
    degraded: u8,
}

impl StateSources {
    /// Fold the sources into one state:
    /// `stopping > writing > working > degraded > idle`. Activity
    /// outranks the degraded condition so a long ingest reads
    /// `working`, and the condition resurfaces once the daemon
    /// quiesces.
    fn resolve(&self) -> DaemonState {
        if self.stopping {
            DaemonState::Stopping
        } else if self.rpc_write {
            DaemonState::Writing
        } else if self.working_jobs > 0 {
            DaemonState::Working
        } else if self.degraded != 0 {
            DaemonState::Degraded
        } else {
            DaemonState::Idle
        }
    }
}

/// Cloneable broadcast handle. Each control-plane connection
/// subscribes once at `events.subscribe` time and consumes the
/// receiver for the lifetime of the connection.
///
/// The daemon lifecycle state is derived, not assigned: producers flip
/// their own source ([`set_rpc_write`](Self::set_rpc_write),
/// [`job_guard`](Self::job_guard), [`set_degraded`](Self::set_degraded),
/// [`set_stopping`](Self::set_stopping)) and the handle folds all
/// sources into one [`DaemonState`] by precedence, publishing a
/// `daemon.state` event only when the folded value changes. Concurrent
/// activities therefore cannot clobber each other's transitions — an
/// RPC write ending while a queue job still runs falls back to
/// `working`, not `idle`.
#[derive(Debug, Clone)]
pub struct EventStreamHandle {
    tx: broadcast::Sender<Event>,
    state: Arc<DaemonStateFlag>,
    sources: Arc<std::sync::Mutex<StateSources>>,
}

impl EventStreamHandle {
    pub fn new(capacity: usize, state: Arc<DaemonStateFlag>) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            state,
            sources: Arc::new(std::sync::Mutex::new(StateSources::default())),
        }
    }

    /// Publish a single event. A `send` with no receivers is not an
    /// error; the next subscriber starts fresh from the next event.
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Hand out a fresh broadcast receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Read the latest daemon lifecycle state for `status` /
    /// `events.subscribe` snapshots.
    pub fn current_state(&self) -> DaemonState {
        self.state.load()
    }

    /// Mutate the sources under the lock, re-resolve, and broadcast
    /// the transition when the folded state changed. Store and publish
    /// happen under the same lock so the event order on the broadcast
    /// matches the state order every subscriber observes.
    fn update_sources(&self, mutate: impl FnOnce(&mut StateSources)) {
        let mut sources = self.sources.lock().expect("state sources mutex poisoned");
        mutate(&mut sources);
        let resolved = sources.resolve();
        if self.state.load() != resolved {
            self.state.store(resolved);
            self.publish(Event::DaemonState(resolved));
        }
    }

    /// Mark shutdown. Terminal: outranks every other source and is
    /// never cleared.
    pub fn set_stopping(&self) {
        self.update_sources(|s| s.stopping = true);
    }

    /// Flip the RPC write-session source. `true` while `run_write`
    /// holds the runtime-wide write mutex.
    pub fn set_rpc_write(&self, active: bool) {
        self.update_sources(|s| s.rpc_write = active);
    }

    /// Count a queue job as executing for the guard's lifetime. The
    /// guard's `Drop` decrements the count, so a panicking or aborted
    /// job cannot leave the daemon stranded in `working`.
    pub fn job_guard(&self) -> WorkingGuard {
        self.update_sources(|s| s.working_jobs += 1);
        WorkingGuard {
            handle: self.clone(),
        }
    }

    /// Set or clear one degraded cause. The daemon reads `degraded`
    /// while any cause is set and nothing outranks it.
    pub fn set_degraded(&self, cause: DegradedCause, active: bool) {
        self.update_sources(|s| {
            if active {
                s.degraded |= cause.bit();
            } else {
                s.degraded &= !cause.bit();
            }
        });
    }
}

impl Default for EventStreamHandle {
    fn default() -> Self {
        Self::new(
            DEFAULT_EVENT_CHANNEL_CAPACITY,
            Arc::new(DaemonStateFlag::default()),
        )
    }
}

/// RAII token for one executing queue job, handed out by
/// [`EventStreamHandle::job_guard`]. Dropping it releases the job's
/// contribution to the `working` state.
#[derive(Debug)]
pub struct WorkingGuard {
    handle: EventStreamHandle,
}

impl Drop for WorkingGuard {
    fn drop(&mut self) {
        self.handle
            .update_sources(|s| s.working_jobs = s.working_jobs.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts_rs::TS;

    #[test]
    fn event_ts_export_contains_every_channel() {
        Event::export_all().expect("ts-rs export Event");
        let dir = std::env::var("TS_RS_EXPORT_DIR").expect("TS_RS_EXPORT_DIR not set");
        let path = std::path::PathBuf::from(dir).join("Event.ts");
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for ch in [
            "daemon.state",
            "queue.tick",
            "worker.progress",
            "library.changed",
            "mcp.availability",
            "log",
        ] {
            assert!(
                contents.contains(ch),
                "Event.ts missing channel {ch}:\n{contents}"
            );
        }
    }

    #[test]
    fn daemon_state_round_trips_through_u8() {
        for s in [
            DaemonState::Idle,
            DaemonState::Writing,
            DaemonState::Degraded,
            DaemonState::Stopping,
            DaemonState::Working,
        ] {
            assert_eq!(DaemonState::from_u8(s.as_u8()), s);
        }
    }

    #[test]
    fn unknown_discriminant_collapses_to_idle() {
        assert_eq!(DaemonState::from_u8(99), DaemonState::Idle);
    }

    #[test]
    fn resolver_folds_sources_by_precedence() {
        // (stopping, rpc_write, working_jobs, degraded bits) -> state
        let table = [
            ((false, false, 0, 0), DaemonState::Idle),
            ((false, false, 0, 1), DaemonState::Degraded),
            ((false, false, 1, 0), DaemonState::Working),
            ((false, false, 1, 1), DaemonState::Working),
            ((false, true, 0, 0), DaemonState::Writing),
            ((false, true, 1, 3), DaemonState::Writing),
            ((true, true, 1, 3), DaemonState::Stopping),
            ((true, false, 0, 0), DaemonState::Stopping),
        ];
        for ((stopping, rpc_write, working_jobs, degraded), expected) in table {
            let sources = StateSources {
                stopping,
                rpc_write,
                working_jobs,
                degraded,
            };
            assert_eq!(sources.resolve(), expected, "sources: {sources:?}");
        }
    }

    #[test]
    fn source_flips_publish_only_on_folded_change() {
        let handle = EventStreamHandle::default();
        let mut rx = handle.subscribe();
        handle.set_rpc_write(false);
        assert!(rx.try_recv().is_err(), "no-op flip must not publish");
        handle.set_stopping();
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, Event::DaemonState(DaemonState::Stopping)));
        handle.set_stopping();
        assert!(rx.try_recv().is_err(), "repeated stop must not re-publish");
    }

    #[test]
    fn job_guard_brackets_working_and_survives_overlap_with_write() {
        let state = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let handle = EventStreamHandle::new(8, state.clone());
        let mut rx = handle.subscribe();

        let guard = handle.job_guard();
        assert_eq!(state.load(), DaemonState::Working);

        // An RPC write session outranks the running job...
        handle.set_rpc_write(true);
        assert_eq!(state.load(), DaemonState::Writing);
        // ...and its end falls back to the still-running job, not idle.
        handle.set_rpc_write(false);
        assert_eq!(state.load(), DaemonState::Working);

        drop(guard);
        assert_eq!(state.load(), DaemonState::Idle);

        let seen: Vec<DaemonState> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                Event::DaemonState(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(
            seen,
            [
                DaemonState::Working,
                DaemonState::Writing,
                DaemonState::Working,
                DaemonState::Idle,
            ]
        );
    }

    #[test]
    fn overlapping_job_guards_stay_working_until_the_last_drops() {
        let state = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let handle = EventStreamHandle::new(8, state.clone());
        let first = handle.job_guard();
        let second = handle.job_guard();
        drop(first);
        assert_eq!(state.load(), DaemonState::Working);
        drop(second);
        assert_eq!(state.load(), DaemonState::Idle);
    }

    #[test]
    fn degraded_causes_set_and_clear_independently() {
        let state = Arc::new(DaemonStateFlag::new(DaemonState::Idle));
        let handle = EventStreamHandle::new(8, state.clone());
        handle.set_degraded(DegradedCause::QueueFailurePause, true);
        handle.set_degraded(DegradedCause::RerankCrashloop, true);
        assert_eq!(state.load(), DaemonState::Degraded);
        handle.set_degraded(DegradedCause::QueueFailurePause, false);
        assert_eq!(state.load(), DaemonState::Degraded);
        handle.set_degraded(DegradedCause::RerankCrashloop, false);
        assert_eq!(state.load(), DaemonState::Idle);
    }
}
