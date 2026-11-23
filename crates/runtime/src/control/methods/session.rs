// SPDX-License-Identifier: Apache-2.0

//! The session tools: what MCP answers about the daemon itself — its
//! identity and mounted libraries, the tail of its log, a snapshot of
//! its queue — and the graceful shutdown it can be asked for.
//!
//! The control plane answers the same questions with methods of its
//! own (`daemon.status`, `logs.tail`, `queue.list`, `daemon.shutdown`)
//! and does not serve these names; a control-plane call to one is told
//! which method to use instead.
//!
//! Each tool body here returns its typed result or a [`SessionError`];
//! the transport serializes the one and maps the other through
//! [`crate::control::error_map::session_err`].

use std::sync::{Arc, Mutex};
use std::time::Instant;

use bookrack_core::queue::{JobState, QueueState};
use bookrack_core::{DaemonIdentity, Problem};
use bookrack_obs::{LogEvent, LogStreamHandle};
use bookrack_ops::dto::UnknownFilterValue;
use bookrack_ops::registry::RegistryError;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::DaemonRegistry;
use crate::control::events::EventStreamHandle;

/// Why a session tool failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The library registry could not be read.
    #[error("read library registry")]
    Registry(#[source] RegistryError),
    /// The queue snapshot was asked for a state no job can be in.
    #[error("unknown queue state filter value {:?}", .0.value)]
    UnknownState(UnknownFilterValue),
    /// The queue snapshot's lock was poisoned by a panic while held.
    #[error("the queue state lock is poisoned")]
    QueueStateLock,
}

/// Arguments for `session.info`. The tool takes no inputs.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SessionInfoParams {}

/// Response shape returned by `session.info`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionInfoResult {
    /// A UUIDv7 the daemon drew when it started; distinct for every
    /// start.
    pub instance_id: String,
    /// `bookrack` workspace version the daemon was built from.
    pub version: String,
    /// The daemon's process id.
    pub pid: u32,
    /// The runtime directory holding the daemon's session lock.
    pub runtime_dir: String,
    /// The control-plane endpoint the daemon answers on, when it has
    /// one.
    pub control_sock: Option<String>,
    /// When the daemon entered service, RFC 3339.
    pub started_at: String,
    /// Wall-clock seconds since the daemon started.
    pub uptime_seconds: u64,
    /// Every library this session has mounted, one row each, sorted by
    /// name.
    pub libraries: Vec<SessionLibrary>,
    /// Name of the row in `libraries` marked `default` — the library a
    /// tool call without a `library` selector resolves to — or empty
    /// when no row carries the mark.
    pub default_library: String,
    /// The registered libraries bring-up did not mount, one row each,
    /// sorted by name, each with its root and the reason; empty when
    /// every registered library came up. Read in the same instant as
    /// `libraries`, so no name appears in both.
    pub unmounted: Vec<SessionUnmounted>,
    /// MCP listener address (`host:port`), or `disabled` when the
    /// daemon is running without `/mcp`.
    pub mcp_addr: String,
    /// Ollama HTTP endpoint the daemon will reach.
    pub ollama_url: String,
}

/// One mounted library in a [`SessionInfoResult`]. The control plane's
/// `status.served` rows carry the same three fields plus a `primary`
/// bit, which an MCP client has no use for: it reaches a library by
/// name or through the default.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionLibrary {
    /// The library's registered short name.
    pub name: String,
    /// The library's own data root (rendered), taken from its handle's
    /// configuration.
    pub data_dir: String,
    /// Whether this is the library a tool call without a `library`
    /// selector resolves to.
    pub default: bool,
}

/// One registered library the daemon did not mount, as `session.info`
/// reports it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionUnmounted {
    /// The library's registered short name.
    pub name: String,
    /// The data root the registry records for it (rendered).
    pub data_dir: String,
    /// Why bring-up did not mount it.
    pub reason: SessionProblem,
}

/// A three-part diagnostic as `session.info` carries it: the same four
/// fields a `Problem` serializes to, mirrored so the tool's schema can
/// describe them.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionProblem {
    /// One line stating what failed.
    pub summary: String,
    /// Implementation-level evidence for the summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the operator should do next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Whether the same action, repeated unchanged, may succeed.
    pub retryable: bool,
}

/// Arguments for `session.logs_tail`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SessionLogsTailParams {
    /// Maximum number of log events to return, from the most recent.
    /// Capped server-side at [`bookrack_obs::stream::TAIL_REQUEST_MAX`];
    /// defaults to [`bookrack_obs::stream::TAIL_REQUEST_DEFAULT`] when
    /// omitted.
    #[serde(default)]
    pub n: Option<usize>,
}

/// Response shape returned by `session.logs_tail`: a slice of the
/// daemon's in-memory log ring buffer, oldest first.
#[derive(Debug, Serialize)]
pub struct SessionLogsTailResult {
    /// The events themselves.
    pub events: Vec<LogEvent>,
    /// How many events the tool returned (`<= n` capped by buffer
    /// occupancy).
    pub returned: usize,
}

/// Arguments for `session.queue_status`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SessionQueueStatusParams {
    /// List only jobs in this lifecycle state in `recent`, under the
    /// same cap and order. The names are the ones a row reports in its
    /// `state`: `pending`, `running`, `done`, `skipped_duplicate`,
    /// `needs_ocr`, `failed`, `cancelled`. The counts cover the whole
    /// queue either way. An unrecognised name is refused rather than
    /// dropped.
    #[serde(default)]
    pub state: Option<String>,
}

/// Compact projection of one queued ingest job, used in the `recent`
/// list returned by `session.queue_status`.
#[derive(Debug, Serialize)]
pub struct QueueJobSummary {
    /// UUIDv7 string identifying the job.
    pub id: String,
    /// Lifecycle state, lowercased to match the persisted serde form.
    pub state: String,
    /// Library name the job runs against.
    pub library: String,
    /// The file the job reads; absent for a job that reads none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// When the job reached its terminal state; absent while it has not
    /// reached one, as for a pending or running job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The failure the job recorded — a `failed` job's cause, as one
    /// flattened chain — or absent when it recorded none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The failure as the daemon classified it — summary, detail, hint,
    /// `retryable` — when it did; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<Problem>,
}

/// Response shape returned by `session.queue_status`: counts by state
/// and a small recent-job tail.
#[derive(Debug, Serialize)]
pub struct SessionQueueStatusResult {
    /// Whether the worker is currently paused.
    pub paused: bool,
    /// Jobs in `Pending` state.
    pub pending: usize,
    /// Jobs in `Running` state.
    pub running: usize,
    /// Jobs in `Done` state.
    pub done: usize,
    /// Jobs in `SkippedDuplicate` state: the worker short-circuited
    /// because the source was already in the catalog with matching
    /// stamps, so no new intake row was written.
    pub skipped_duplicate: usize,
    /// Jobs in `NeedsOcr` state: the source had no usable text layer and
    /// was registered as a `needs_ocr` anchor until its text is ingested.
    pub needs_ocr: usize,
    /// Jobs in `Failed` state.
    pub failed: usize,
    /// Jobs in `Cancelled` state.
    pub cancelled: usize,
    /// The [`SESSION_QUEUE_STATUS_RECENT`] most recently enqueued jobs,
    /// newest first; with `state`, the most recently enqueued jobs in
    /// that state.
    pub recent: Vec<QueueJobSummary>,
}

/// Cap on the `recent` field returned by `session.queue_status`.
// setting: mcp.queue_status_recent
pub const SESSION_QUEUE_STATUS_RECENT: usize = 10;

/// Arguments for `session.shutdown`. The tool takes no inputs.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SessionShutdownParams {}

/// Response shape returned by `session.shutdown`.
#[derive(Debug, Serialize)]
pub struct SessionShutdownResult {
    /// Always `shutting down`: the signal is sent, and the daemon winds
    /// down after the reply.
    pub status: &'static str,
}

/// The daemon's runtime summary: its identity, the libraries mounted
/// and not mounted, the default, and the two endpoints of the process.
///
/// One registry read yields the mounted rows, the default name, and
/// the unmounted rows, so a default pointer moved mid-call cannot leave
/// the card naming one default and marking another, and a mount landing
/// mid-call cannot list one name twice.
pub fn info(
    registry: &DaemonRegistry,
    identity: &DaemonIdentity,
    started_at: Instant,
    mcp_addr: &str,
    ollama_url: &str,
) -> Result<SessionInfoResult, SessionError> {
    let snapshot = registry.snapshot().map_err(SessionError::Registry)?;
    let libraries: Vec<SessionLibrary> = snapshot
        .mounted
        .into_iter()
        .map(|s| SessionLibrary {
            name: s.name,
            data_dir: format!("{}", s.data_dir.display()),
            default: s.is_default,
        })
        .collect();
    let unmounted: Vec<SessionUnmounted> = snapshot
        .unmounted
        .iter()
        .map(|library| SessionUnmounted {
            name: String::from(library.name()),
            data_dir: format!("{}", library.data_dir().display()),
            reason: SessionProblem {
                summary: library.reason().summary.clone(),
                detail: library.reason().data.detail.clone(),
                hint: library.reason().data.hint.clone(),
                retryable: library.reason().data.retryable,
            },
        })
        .collect();
    let default_library = libraries
        .iter()
        .find(|row| row.default)
        .map(|row| row.name.clone())
        .unwrap_or_default();
    Ok(SessionInfoResult {
        instance_id: identity.instance_id.clone(),
        version: identity.version.clone(),
        pid: identity.pid,
        runtime_dir: identity.runtime_dir.clone(),
        control_sock: identity.control_sock.clone(),
        started_at: identity.started_at.clone(),
        uptime_seconds: started_at.elapsed().as_secs(),
        libraries,
        default_library,
        unmounted,
        mcp_addr: String::from(mcp_addr),
        ollama_url: String::from(ollama_url),
    })
}

/// The most recent log events from the daemon's in-memory ring buffer,
/// oldest first within the slice.
pub fn logs_tail(
    log_stream: &LogStreamHandle,
    params: &SessionLogsTailParams,
) -> SessionLogsTailResult {
    let n = params
        .n
        .unwrap_or(bookrack_obs::stream::TAIL_REQUEST_DEFAULT)
        .min(bookrack_obs::stream::TAIL_REQUEST_MAX);
    let events = log_stream.tail(n);
    let returned = events.len();
    SessionLogsTailResult { events, returned }
}

/// The queue's counts by lifecycle state, and its most recently
/// enqueued jobs, optionally only those in one state. The filter
/// applies before the cap, so an early failure in a long batch stays
/// reachable. A host that spawns no queue worker hands in an inert
/// snapshot, which reports an empty queue.
pub fn queue_status(
    queue_state: &Arc<Mutex<QueueState>>,
    params: &SessionQueueStatusParams,
) -> Result<SessionQueueStatusResult, SessionError> {
    let wanted = params
        .state
        .as_deref()
        .map(parse_job_state)
        .transpose()
        .map_err(SessionError::UnknownState)?;
    let state = queue_state
        .lock()
        .map_err(|_| SessionError::QueueStateLock)?;
    let mut counts = [0usize; 7];
    for job in &state.jobs {
        let idx = match job.state {
            JobState::Pending => 0,
            JobState::Running => 1,
            JobState::Done => 2,
            JobState::SkippedDuplicate => 3,
            JobState::NeedsOcr => 4,
            JobState::Failed => 5,
            JobState::Cancelled => 6,
        };
        counts[idx] += 1;
    }
    let recent: Vec<QueueJobSummary> = state
        .jobs
        .iter()
        .rev()
        .filter(|j| wanted.is_none_or(|state| j.state == state))
        .take(SESSION_QUEUE_STATUS_RECENT)
        .map(|j| QueueJobSummary {
            id: j.id.clone(),
            // The stable snake_case wire token, not `Debug`, so a
            // multi-word state reads `needs_ocr` rather than
            // `needsocr`.
            state: String::from(j.state.as_wire_str()),
            library: j.library.clone(),
            path: j.source_path().map(|path| format!("{}", path.display())),
            finished_at: j.finished_at,
            error: j.error.clone(),
            problem: j.problem.clone(),
        })
        .collect();
    Ok(SessionQueueStatusResult {
        paused: state.paused,
        pending: counts[0],
        running: counts[1],
        done: counts[2],
        skipped_duplicate: counts[3],
        needs_ocr: counts[4],
        failed: counts[5],
        cancelled: counts[6],
        recent,
    })
}

/// Ask the daemon to shut down gracefully: publish the `stopping` state
/// to control-plane subscribers, then fire the session-wide signal the
/// signal listener, the control socket, the queue worker, and the MCP
/// listener all subscribe to. Returns as soon as the signal is sent;
/// the shutdown itself runs in the daemon's main loop.
pub fn shutdown(
    event_stream: &EventStreamHandle,
    shutdown_tx: &broadcast::Sender<()>,
) -> SessionShutdownResult {
    crate::control::events::initiate_shutdown(event_stream, shutdown_tx);
    SessionShutdownResult {
        status: "shutting down",
    }
}

/// Parse a `session.queue_status` `state` filter from the wire token a
/// row reports ([`JobState::as_wire_str`]).
fn parse_job_state(value: &str) -> Result<JobState, UnknownFilterValue> {
    JobState::ALL
        .into_iter()
        .find(|state| state.as_wire_str() == value)
        .ok_or_else(|| UnknownFilterValue {
            parameter: "state",
            value: String::from(value),
            accepted: JobState::ALL.map(JobState::as_wire_str).to_vec(),
        })
}

#[cfg(test)]
mod tests {
    use bookrack_core::ItemKind;
    use bookrack_core::queue::{IngestJob, JobOp, Priority, QueueJob};
    use serde_json::{Value, json};
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::control::events::{DaemonState, Event};
    use crate::control::methods::{logs, reads, test_method_context};

    fn job(n: u32, state: JobState) -> QueueJob {
        let at =
            chrono::DateTime::from_timestamp(1_700_000_000 + i64::from(n), 0).expect("timestamp");
        QueueJob {
            id: format!("job-{n:02}"),
            library: "alpha".to_owned(),
            priority: Priority::Normal,
            kind: ItemKind::Book,
            op: JobOp::Ingest(IngestJob::file(format!("/srv/books/{n:02}.epub"))),
            state,
            queued_at: at,
            started_at: Some(at),
            finished_at: Some(at),
            error: None,
            problem: None,
            merged_into: None,
        }
    }

    /// The tool tails the ring the control plane's `logs.tail` reads,
    /// and returns the same events under the same cap.
    #[test]
    fn the_log_tail_matches_logs_tail() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), Some("alpha"));
        let subscriber = tracing_subscriber::registry().with(
            bookrack_obs::stream::BroadcastLayer::new(ctx.log_stream.clone()),
        );
        tracing::subscriber::with_default(subscriber, || {
            for n in 0..5 {
                tracing::info!(n, "an event the ring keeps");
            }
        });

        let tool = logs_tail(&ctx.log_stream, &SessionLogsTailParams { n: Some(3) });
        let method = logs::tail(&Some(json!({ "n": 3 })), &ctx).expect("logs.tail");
        assert_eq!(tool.returned, 3);
        assert_eq!(
            serde_json::to_value(&tool).expect("serialize"),
            method,
            "the tool and the method read one ring"
        );
    }

    /// The tool's counts are `queue.list`'s summary, and `state`
    /// narrows `recent` without narrowing the counts.
    #[test]
    fn the_queue_snapshot_counts_what_queue_list_counts() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), Some("alpha"));
        ctx.queue_state.lock().unwrap().jobs = vec![
            job(1, JobState::Failed),
            job(2, JobState::Done),
            job(3, JobState::Done),
            job(4, JobState::Pending),
        ];

        let all = queue_status(&ctx.queue_state, &SessionQueueStatusParams::default())
            .expect("the snapshot");
        let list = reads::queue_list(&None, &ctx).expect("queue.list");
        let summary = &list["summary"];
        for (state, count) in [
            ("pending", all.pending),
            ("running", all.running),
            ("done", all.done),
            ("skipped_duplicate", all.skipped_duplicate),
            ("needs_ocr", all.needs_ocr),
            ("failed", all.failed),
            ("cancelled", all.cancelled),
        ] {
            assert_eq!(summary[state], json!(count), "{state}: {summary}");
        }
        let ids: Vec<&str> = all.recent.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            ["job-04", "job-03", "job-02", "job-01"],
            "newest first"
        );

        let failed = queue_status(
            &ctx.queue_state,
            &SessionQueueStatusParams {
                state: Some("failed".to_owned()),
            },
        )
        .expect("the filtered snapshot");
        let ids: Vec<&str> = failed.recent.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["job-01"]);
        assert_eq!(failed.done, 2, "the counts cover the whole queue");
    }

    /// A state no job can be in is refused as caller input, naming the
    /// states that exist.
    #[test]
    fn an_unknown_state_filter_is_refused_as_caller_input() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), Some("alpha"));
        let err = queue_status(
            &ctx.queue_state,
            &SessionQueueStatusParams {
                state: Some("stuck".to_owned()),
            },
        )
        .expect_err("an unknown state");
        let rpc = crate::control::error_map::session_err(err);
        assert_eq!(rpc.code, crate::control::jsonrpc::INVALID_PARAMS);
        assert!(rpc.message.contains("stuck"), "{}", rpc.message);
        let hint = rpc.data.as_ref().and_then(|data| data["hint"].as_str());
        assert!(
            hint.is_some_and(|hint| hint.contains("needs_ocr")),
            "{:?}",
            rpc.data
        );
    }

    /// A shutdown publishes `stopping` before the signal goes out, so a
    /// subscriber hears why the daemon is going away.
    #[test]
    fn a_shutdown_announces_stopping_before_the_signal() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), Some("alpha"));
        let mut events = ctx.event_stream.subscribe();
        let mut signal = ctx.shutdown_tx.subscribe();

        let reply = shutdown(&ctx.event_stream, &ctx.shutdown_tx);
        assert_eq!(
            serde_json::to_value(&reply).expect("serialize"),
            json!({ "status": "shutting down" })
        );
        assert!(signal.try_recv().is_ok(), "the shutdown signal was sent");
        let mut saw_stopping = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, Event::DaemonState(DaemonState::Stopping)) {
                saw_stopping = true;
            }
        }
        assert!(saw_stopping, "stopping was published");
    }

    /// The card lists the mounted library as the default and carries the
    /// two endpoints it was handed.
    #[test]
    fn the_session_card_lists_the_mounted_default_and_the_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_method_context(dir.path(), Some("alpha"));
        let card = info(
            &ctx.registry,
            &ctx.identity,
            Instant::now(),
            "127.0.0.1:9",
            "http://127.0.0.1:11434",
        )
        .expect("the card");
        let value: Value = serde_json::to_value(&card).expect("serialize");
        assert_eq!(value["default_library"], "alpha", "{value}");
        assert_eq!(value["libraries"][0]["name"], "alpha", "{value}");
        assert_eq!(value["libraries"][0]["default"], true, "{value}");
        assert_eq!(value["mcp_addr"], "127.0.0.1:9", "{value}");
        assert_eq!(value["instance_id"], ctx.identity.instance_id, "{value}");
    }

    /// The recent-jobs cap is registered as this crate's compiled-in
    /// value, under the key its marker names.
    #[test]
    fn the_recent_cap_is_a_runtime_setting() {
        let row = crate::fixed::FIXED_SETTINGS
            .iter()
            .find(|row| row.key == "mcp.queue_status_recent")
            .expect("the cap is registered");
        assert_eq!(row.owner, "runtime");
        assert_eq!((row.value)(), format!("{SESSION_QUEUE_STATUS_RECENT}"));
    }
}
