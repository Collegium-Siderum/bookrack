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
//! The runner reports that a job has been pulled and is processing.
//! The one pipeline hook it has is the book embedding pass, whose
//! per-batch callback [`embed_batch_reporter`] turns into throttled
//! [`Stage::Embed`] fractions.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use bookrack_core::ItemKind;

use super::events::{Event, EventStreamHandle, Stage, WorkerProgress};

/// Shortest gap between two embed fractions published for one job.
// setting: internal -- how often a progress fraction is published, not
// how the embedding pass runs
pub const EMBED_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

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

/// Decides which embed batches are published. The first batch and the
/// last (`consumed == total`) always pass; one in between passes once
/// `interval` has elapsed since the last batch that passed.
#[derive(Debug)]
pub struct EmbedThrottle {
    interval: Duration,
    last: Option<Instant>,
}

impl EmbedThrottle {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    pub fn should_emit(&mut self, consumed: usize, total: usize, now: Instant) -> bool {
        let due = match self.last {
            None => true,
            Some(last) => consumed >= total || now.duration_since(last) >= self.interval,
        };
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// Callback for the book embedding pass that reports
/// `consumed / total` to `sink` as [`Stage::Embed`], through an
/// [`EmbedThrottle`] with `interval`.
pub fn embed_batch_reporter(
    sink: &dyn ProgressSink,
    interval: Duration,
) -> impl Fn(usize, usize) + Send + Sync + '_ {
    let throttle = Mutex::new(EmbedThrottle::new(interval));
    move |consumed, total| {
        let due = throttle
            .lock()
            .map(|mut t| t.should_emit(consumed, total, Instant::now()))
            .unwrap_or(false);
        if due && total > 0 {
            sink.report(Stage::Embed, Some(consumed as f32 / total as f32), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recording(Mutex<Vec<(Stage, Option<f32>)>>);

    impl ProgressSink for Recording {
        fn report(&self, stage: Stage, progress: Option<f32>, _message: Option<String>) {
            self.0.lock().unwrap().push((stage, progress));
        }
    }

    fn feed(interval: Duration, total: usize) -> Vec<(Stage, Option<f32>)> {
        let sink = Recording::default();
        let report = embed_batch_reporter(&sink, interval);
        for consumed in 1..=total {
            report(consumed, total);
        }
        drop(report);
        sink.0.into_inner().unwrap()
    }

    #[test]
    fn with_no_interval_every_batch_is_reported() {
        assert_eq!(
            feed(Duration::ZERO, 4),
            vec![
                (Stage::Embed, Some(0.25)),
                (Stage::Embed, Some(0.5)),
                (Stage::Embed, Some(0.75)),
                (Stage::Embed, Some(1.0)),
            ]
        );
    }

    #[test]
    fn a_long_interval_keeps_only_the_first_and_last_batch() {
        assert_eq!(
            feed(Duration::from_secs(3600), 5),
            vec![(Stage::Embed, Some(0.2)), (Stage::Embed, Some(1.0))]
        );
    }

    #[test]
    fn a_single_batch_is_reported_once_as_complete() {
        assert_eq!(
            feed(Duration::from_secs(3600), 1),
            vec![(Stage::Embed, Some(1.0))]
        );
    }

    #[test]
    fn an_intermediate_batch_passes_once_the_interval_elapses() {
        let start = Instant::now();
        let mut throttle = EmbedThrottle::new(Duration::from_millis(100));
        assert!(throttle.should_emit(1, 10, start));
        assert!(!throttle.should_emit(2, 10, start + Duration::from_millis(99)));
        assert!(throttle.should_emit(3, 10, start + Duration::from_millis(100)));
        assert!(!throttle.should_emit(4, 10, start + Duration::from_millis(150)));
        assert!(throttle.should_emit(10, 10, start + Duration::from_millis(151)));
    }
}
