// SPDX-License-Identifier: Apache-2.0

//! Progress reporting sink threaded into the queue runner.
//!
//! The queue worker publishes per-job progress through a
//! [`ProgressSink`] so the runner does not depend on the broadcast
//! handle directly. [`NoopProgressSink`] is used by CLI call sites
//! that do not have a broadcast attached; [`EventProgressSink`]
//! bridges to [`EventStreamHandle`] and is the production wiring used
//! by `bookrack run`.
//!
//! The pipelines expose no stage hooks to the runner, so the sink
//! reports only what the runner itself observes: that a job has been
//! pulled and is processing.

use bookrack_core::ItemKind;

use super::events::{Event, EventStreamHandle, Stage, WorkerProgress};

/// Receiver for per-job progress notifications. Implementations are
/// expected to be inexpensive on the hot path; the queue worker calls
/// [`ProgressSink::report`] at every position it can observe.
pub trait ProgressSink: Send + Sync {
    fn report(&self, stage: Stage, progress: Option<f32>, message: Option<String>);
}

/// Drops every notification. Use from CLI call sites that do not
/// subscribe to the daemon broadcast.
pub struct NoopProgressSink;

impl ProgressSink for NoopProgressSink {
    fn report(&self, _stage: Stage, _progress: Option<f32>, _message: Option<String>) {}
}

/// Forwards every notification onto the daemon-wide broadcast as a
/// [`Event::WorkerProgress`] tagged with the job id and pipeline kind
/// captured at construction.
pub struct EventProgressSink {
    job_id: String,
    kind: ItemKind,
    events: EventStreamHandle,
}

impl EventProgressSink {
    pub fn new(job_id: String, kind: ItemKind, events: EventStreamHandle) -> Self {
        Self {
            job_id,
            kind,
            events,
        }
    }
}

impl ProgressSink for EventProgressSink {
    fn report(&self, stage: Stage, progress: Option<f32>, message: Option<String>) {
        self.events.publish(Event::WorkerProgress(WorkerProgress {
            job_id: self.job_id.clone(),
            kind: self.kind,
            stage,
            stage_progress: progress,
            message,
        }));
    }
}
