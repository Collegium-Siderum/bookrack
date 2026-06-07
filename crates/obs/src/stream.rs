// SPDX-License-Identifier: Apache-2.0

//! In-process log fan-out for `tracing` events.
//!
//! [`BroadcastLayer`] is a third `tracing` layer (alongside the
//! human-readable stderr layer and the JSON file layer) that packs each
//! event into a serialisable [`LogEvent`] and pushes it through a shared
//! [`LogStreamHandle`]. The handle owns two parallel sinks for the same
//! event stream:
//!
//! * an `Arc<Mutex<VecDeque<LogEvent>>>` ring buffer — bounded, FIFO —
//!   that [`LogStreamHandle::tail`] reads from for a one-shot "last N
//!   events" snapshot (`session.logs_tail`, REPL `:logs tail`);
//! * a `tokio::sync::broadcast::Sender<LogEvent>` that
//!   [`LogStreamHandle::subscribe`] hands out receivers from for live
//!   streaming consumers (`/session/logs` SSE endpoint, future GUI side
//!   panel).
//!
//! The handle is cheap to clone and `Send + Sync`, so every consumer
//! (MCP server state, the REPL, the SSE handler) just keeps a copy.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Default capacity of the broadcast channel handed out by
/// [`LogStreamHandle::default`]. Sized for handful-of-subscribers
/// scenarios (REPL builtin, SSE clients) where every consumer keeps
/// up; lagging subscribers get a `Lagged` error and resume from the
/// next event rather than blocking the producer.
pub const DEFAULT_BROADCAST_CAPACITY: usize = 512;

/// Default capacity of the ring buffer backing [`LogStreamHandle::tail`].
/// 2048 events covers the most recent minutes of a busy ingest run
/// without unbounded memory growth.
pub const DEFAULT_TAIL_CAPACITY: usize = 2048;

/// A single `tracing` event captured by [`BroadcastLayer`].
///
/// The shape is deliberately close to the JSON file layer's so SSE
/// consumers can forward it verbatim and `session.logs_tail` can return
/// a slice of events without re-formatting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEvent {
    /// Wall-clock timestamp the event passed through the layer.
    pub ts: chrono::DateTime<chrono::Utc>,
    /// `"INFO"` / `"WARN"` / `"ERROR"` / `"DEBUG"` / `"TRACE"`.
    pub level: String,
    /// `module_path!()`-derived target the event was emitted from.
    pub target: String,
    /// The event's `message` field, rendered through `Debug` or `Display`.
    pub message: String,
    /// All other event fields, with values serialised through `Debug`
    /// when they are not natively JSON-compatible.
    pub fields: serde_json::Map<String, serde_json::Value>,
}

/// Clone-able handle to the in-process log stream.
///
/// Built once at startup by `bookrack_obs::init` and shared across
/// every consumer; subsequent clones share the same ring buffer and
/// broadcast channel.
#[derive(Clone)]
pub struct LogStreamHandle {
    tx: broadcast::Sender<LogEvent>,
    tail: Arc<Mutex<VecDeque<LogEvent>>>,
    tail_capacity: usize,
}

impl fmt::Debug for LogStreamHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogStreamHandle")
            .field("subscribers", &self.tx.receiver_count())
            .field("tail_capacity", &self.tail_capacity)
            .finish_non_exhaustive()
    }
}

impl Default for LogStreamHandle {
    fn default() -> Self {
        LogStreamHandle::new(DEFAULT_BROADCAST_CAPACITY, DEFAULT_TAIL_CAPACITY)
    }
}

impl LogStreamHandle {
    /// Construct a new handle with explicit capacities.
    pub fn new(broadcast_capacity: usize, tail_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(broadcast_capacity);
        LogStreamHandle {
            tx,
            tail: Arc::new(Mutex::new(VecDeque::with_capacity(tail_capacity))),
            tail_capacity,
        }
    }

    /// Hand out a new receiver against the broadcast channel. The
    /// receiver yields every event published after this call returns;
    /// missed events while the receiver lags surface as `Lagged`
    /// errors and the receiver resumes from the next live event.
    pub fn subscribe(&self) -> broadcast::Receiver<LogEvent> {
        self.tx.subscribe()
    }

    /// Snapshot the most recent `n` events from the ring buffer,
    /// chronological order (oldest first). Returns at most
    /// `min(n, ring_buffer.len())` events.
    pub fn tail(&self, n: usize) -> Vec<LogEvent> {
        let tail = self.tail.lock().unwrap();
        let take = n.min(tail.len());
        tail.iter().rev().take(take).rev().cloned().collect()
    }

    /// Number of active broadcast receivers — diagnostic only.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Internal: append an event to the ring buffer and publish it on
    /// the broadcast channel. Called by [`BroadcastLayer::on_event`].
    fn push(&self, event: LogEvent) {
        {
            let mut tail = self.tail.lock().unwrap();
            if tail.len() == self.tail_capacity {
                tail.pop_front();
            }
            tail.push_back(event.clone());
        }
        // `send` errors when no receivers are attached; the ring
        // buffer still owns the event for the next `tail()` call.
        let _ = self.tx.send(event);
    }
}

/// `tracing_subscriber::Layer` that mirrors every event into the
/// in-process [`LogStreamHandle`].
///
/// Compose with `tracing_subscriber::Layer::with_filter` to bound the
/// volume — `bookrack_obs::init` filters this layer with the same
/// `EnvFilter` directive as the file layer so SSE clients and the ring
/// buffer see the persisted audit trail, not the noisier raw stream.
pub struct BroadcastLayer {
    handle: LogStreamHandle,
}

impl BroadcastLayer {
    /// Wrap an existing handle so multiple consumers share one ring
    /// buffer and broadcast channel.
    pub fn new(handle: LogStreamHandle) -> Self {
        BroadcastLayer { handle }
    }
}

impl<S> Layer<S> for BroadcastLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.handle.push(pack_event(event));
    }
}

/// Convert a live `tracing::Event` into a serialisable [`LogEvent`].
fn pack_event(event: &Event<'_>) -> LogEvent {
    let mut visitor = FieldVisitor::default();
    event.record(&mut visitor);
    LogEvent {
        ts: chrono::Utc::now(),
        level: event.metadata().level().to_string(),
        target: event.metadata().target().to_string(),
        message: visitor.message.unwrap_or_default(),
        fields: visitor.fields,
    }
}

/// `tracing::field::Visit` that pulls the `message` field out into its
/// own slot and stuffs every other field into a JSON map. Non-JSON
/// types fall through `record_debug` and end up as a `Debug`-rendered
/// string — same behaviour the file layer's JSON formatter applies.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl FieldVisitor {
    fn record_value(&mut self, field: &Field, value: serde_json::Value) {
        if field.name() == "message" {
            if let serde_json::Value::String(s) = value {
                self.message = Some(s);
            } else {
                self.message = Some(value.to_string());
            }
        } else {
            self.fields.insert(field.name().to_string(), value);
        }
    }
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(rendered),
            );
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_value(field, serde_json::Value::String(value.to_string()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_value(field, serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_value(field, serde_json::Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        match serde_json::Number::from_f64(value) {
            Some(n) => self.record_value(field, serde_json::Value::Number(n)),
            None => self.record_value(field, serde_json::Value::String(value.to_string())),
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_value(field, serde_json::Value::Bool(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_value(field, serde_json::Value::String(format!("{value}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::subscriber::with_default;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn tail_returns_chronological_slice_of_last_n() {
        let handle = LogStreamHandle::new(16, 4);
        for i in 0..3 {
            handle.push(LogEvent {
                ts: chrono::Utc::now(),
                level: "INFO".to_string(),
                target: "t".to_string(),
                message: format!("evt-{i}"),
                fields: serde_json::Map::new(),
            });
        }

        let last_two: Vec<_> = handle.tail(2).into_iter().map(|e| e.message).collect();
        assert_eq!(last_two, vec!["evt-1".to_string(), "evt-2".to_string()]);

        // Asking for more than the buffer holds returns everything.
        let all: Vec<_> = handle.tail(10).into_iter().map(|e| e.message).collect();
        assert_eq!(
            all,
            vec![
                "evt-0".to_string(),
                "evt-1".to_string(),
                "evt-2".to_string()
            ]
        );
    }

    #[test]
    fn ring_buffer_drops_oldest_event_at_capacity() {
        let handle = LogStreamHandle::new(16, 2);
        for i in 0..5 {
            handle.push(LogEvent {
                ts: chrono::Utc::now(),
                level: "INFO".to_string(),
                target: "t".to_string(),
                message: format!("evt-{i}"),
                fields: serde_json::Map::new(),
            });
        }

        let kept: Vec<_> = handle.tail(10).into_iter().map(|e| e.message).collect();
        assert_eq!(kept, vec!["evt-3".to_string(), "evt-4".to_string()]);
    }

    #[test]
    fn subscribe_receives_subsequent_pushes() {
        let handle = LogStreamHandle::new(16, 16);
        let mut rx = handle.subscribe();
        handle.push(LogEvent {
            ts: chrono::Utc::now(),
            level: "INFO".to_string(),
            target: "t".to_string(),
            message: "hello".to_string(),
            fields: serde_json::Map::new(),
        });
        let received = rx.try_recv().expect("event delivered");
        assert_eq!(received.message, "hello");
    }

    #[test]
    fn broadcast_layer_captures_event_with_fields() {
        let handle = LogStreamHandle::new(16, 16);
        let subscriber = Registry::default().with(BroadcastLayer::new(handle.clone()));

        with_default(subscriber, || {
            tracing::info!(
                target: "obs_test",
                count = 42_i64,
                flag = true,
                ratio = 0.5_f64,
                "hello world"
            );
        });

        let events = handle.tail(10);
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.level, "INFO");
        assert_eq!(ev.target, "obs_test");
        assert_eq!(ev.message, "hello world");
        assert_eq!(ev.fields.get("count").and_then(|v| v.as_i64()), Some(42));
        assert_eq!(ev.fields.get("flag").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(ev.fields.get("ratio").and_then(|v| v.as_f64()), Some(0.5));
    }

    #[test]
    fn log_event_round_trips_through_json() {
        let mut fields = serde_json::Map::new();
        fields.insert("key".to_string(), serde_json::Value::from(7_i64));
        let ev = LogEvent {
            ts: chrono::Utc::now(),
            level: "WARN".to_string(),
            target: "t".to_string(),
            message: "msg".to_string(),
            fields,
        };
        let encoded = serde_json::to_string(&ev).unwrap();
        let decoded: LogEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.level, ev.level);
        assert_eq!(decoded.target, ev.target);
        assert_eq!(decoded.message, ev.message);
        assert_eq!(decoded.fields, ev.fields);
    }
}
