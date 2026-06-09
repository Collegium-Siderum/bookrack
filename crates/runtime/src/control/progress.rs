// SPDX-License-Identifier: Apache-2.0

//! Progress reporting sink threaded into the queue runner.
//!
//! Phase 2 wires the queue worker through a [`ProgressSink`] so the
//! per-job runner can publish coarse stage markers without depending
//! on the broadcast handle directly. [`NoopProgressSink`] is used by
//! CLI call sites that do not have a broadcast attached;
//! [`EventProgressSink`] bridges to [`EventStreamHandle`] and is the
//! production wiring used by `bookrack run`.
//!
//! Finer-grained progress within a single stage requires hooks the
//! upstream ingest crate does not yet expose, so the sink only emits
//! boundary transitions at the runner level.

use super::events::{Event, EventStreamHandle, Stage, WorkerProgress};

/// Receiver for stage-boundary notifications. Implementations are
/// expected to be inexpensive on the hot path; the queue worker calls
/// [`ProgressSink::report`] at every visible stage transition.
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
/// [`Event::WorkerProgress`] tagged with the job id captured at
/// construction.
pub struct EventProgressSink {
    job_id: String,
    events: EventStreamHandle,
}

impl EventProgressSink {
    pub fn new(job_id: String, events: EventStreamHandle) -> Self {
        Self { job_id, events }
    }
}

impl ProgressSink for EventProgressSink {
    fn report(&self, stage: Stage, progress: Option<f32>, message: Option<String>) {
        self.events.publish(Event::WorkerProgress(WorkerProgress {
            job_id: self.job_id.clone(),
            stage,
            stage_progress: progress,
            message,
        }));
    }
}
