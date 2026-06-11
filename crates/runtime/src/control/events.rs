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
//! Phase 1 emits one channel: `daemon.state` flips between `idle` and
//! `stopping` over the life of the process. Phase 2 will add
//! `queue.tick`, `worker.progress`, `library.changed`, and
//! `mcp.availability`; the [`Event`] enum is the single extension
//! point.

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
        ts(type = "\"pending\" | \"running\" | \"done\" | \"failed\" | \"cancelled\"")
    )]
    pub state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[cfg_attr(test, ts(type = "string"))]
    pub finished_at: DateTime<Utc>,
}

/// Coarse view of the queue at one persisted moment. Each tick
/// follows a `save_atomic` on `.bookrack-queue.json`, so the values
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

/// Cloneable broadcast handle. Each control-plane connection
/// subscribes once at `events.subscribe` time and consumes the
/// receiver for the lifetime of the connection.
#[derive(Debug, Clone)]
pub struct EventStreamHandle {
    tx: broadcast::Sender<Event>,
    state: Arc<DaemonStateFlag>,
}

impl EventStreamHandle {
    pub fn new(capacity: usize, state: Arc<DaemonStateFlag>) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx, state }
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

    /// Move the daemon-state flag and broadcast the transition. No-op
    /// when the target matches the current state, so callers can fire
    /// idempotently.
    pub fn set_state(&self, state: DaemonState) {
        if self.state.load() == state {
            return;
        }
        self.state.store(state);
        self.publish(Event::DaemonState(state));
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
        ] {
            assert_eq!(DaemonState::from_u8(s.as_u8()), s);
        }
    }

    #[test]
    fn unknown_discriminant_collapses_to_idle() {
        assert_eq!(DaemonState::from_u8(99), DaemonState::Idle);
    }

    #[test]
    fn set_state_is_idempotent_and_publishes_only_on_change() {
        let handle = EventStreamHandle::default();
        let mut rx = handle.subscribe();
        handle.set_state(DaemonState::Idle);
        assert!(rx.try_recv().is_err());
        handle.set_state(DaemonState::Stopping);
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, Event::DaemonState(DaemonState::Stopping)));
    }
}
