// SPDX-License-Identifier: Apache-2.0

//! Persistent ingest queue value types.
//!
//! The queue document is owned by the daemon runtime and its readers —
//! the file format, version gate, atomic write, walker, and worker
//! loop all live there. This module carries only the pure serde
//! types that cross crate boundaries: the MCP `session.queue_status`
//! tool reads the same `QueueState` snapshot the CLI mutates, and
//! both ends ship the same `Priority` / `JobState` / `QueueJob` over
//! the wire without duplicating their definitions.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ItemKind, Problem};

/// Schema version embedded in the persisted document. Bumped whenever
/// any field shape, enum variant, or invariant changes.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const QUEUE_SCHEMA_VERSION: u32 = 8;

/// What a reader does with a persisted document, given the
/// `schema_version` it carries.
///
/// The two directions are not symmetric. A document an earlier binary
/// wrote is loaded through [`read_document`], which reads it in the
/// shape that binary wrote and converts it. A document a later
/// binary wrote is refused, because loading it would drop the fields
/// this binary has no field for and the next write would persist the
/// truncated document over the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOpenDecision {
    /// The document is at or below [`QUEUE_SCHEMA_VERSION`]; read it.
    Match,
    /// The document is above [`QUEUE_SCHEMA_VERSION`]; leave it on disk
    /// untouched and report the two versions.
    Refuse,
}

/// Reduce a persisted document's `schema_version` to an open-time
/// verdict. `found > `[`QUEUE_SCHEMA_VERSION`] is the only refusal.
pub fn queue_open_decision(found: u32) -> QueueOpenDecision {
    if found > QUEUE_SCHEMA_VERSION {
        QueueOpenDecision::Refuse
    } else {
        QueueOpenDecision::Match
    }
}

/// Pull order hint for the worker. The first pending job at the
/// highest priority is picked next.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

/// Lifecycle state of a queued job.
///
/// `Done` covers ingest runs that actually wrote to the catalog «new
/// intake or forced re-run», while `SkippedDuplicate` marks jobs whose
/// source was already on file with up-to-date stamps, so the worker
/// short-circuited without changing catalog state. `NeedsOcr` marks a
/// job whose source has no usable text layer: the worker registered it
/// as a `needs_ocr` intake anchor until its text is ingested but wrote no
/// book content. All three are non-failure terminal states, distinct so
/// operators can reconcile queue counts against `library.stats` without
/// cross-referencing sha256; `NeedsOcr` in particular keeps scan sources
/// out of the `Failed` bucket where they do not belong.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Running,
    Done,
    SkippedDuplicate,
    NeedsOcr,
    Failed,
    Cancelled,
}

impl JobState {
    /// Every lifecycle state, in declaration order. Lets a caller walk
    /// the vocabulary instead of transcribing it.
    pub const ALL: [JobState; 7] = [
        JobState::Pending,
        JobState::Running,
        JobState::Done,
        JobState::SkippedDuplicate,
        JobState::NeedsOcr,
        JobState::Failed,
        JobState::Cancelled,
    ];

    /// The stable wire token for this state — the same `snake_case`
    /// string serde emits. Kept as an explicit map so string consumers
    /// (queue rendering, the MCP session tool) share one source of
    /// truth instead of re-deriving the token from `Debug`.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            JobState::Pending => "pending",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::SkippedDuplicate => "skipped_duplicate",
            JobState::NeedsOcr => "needs_ocr",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

/// What a queued job does. The row around it — id, library, kind,
/// priority, lifecycle — is the same for every operation; everything
/// only one operation reads sits inside its variant.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobOp {
    /// Take one source into the library through the pipeline the row's
    /// `kind` names.
    Ingest(IngestJob),
}

/// The payload of [`JobOp::Ingest`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IngestJob {
    /// The file the job reads; [`Self::input`] says what it is. Resolved
    /// when the job was enqueued; the worker does not re-resolve
    /// relative paths.
    pub path: PathBuf,
    /// Force a fresh ingest even when the source's noop-if-up-to-date
    /// check would otherwise short-circuit.
    pub force: bool,
    /// Park the book at STRUCTURE when the audit verdict is
    /// `needs_work`, skipping CHUNK and EMBED until a curator resumes
    /// it.
    pub hold_for_metadata: bool,
    /// Book-side audit profile override for this job. When `None`, the
    /// worker uses the daemon's default profile baked into the queue
    /// params template at startup. When `Some(name)`, the worker
    /// reloads the named profile per job and substitutes it into the
    /// ingest params before dispatch. Paper-side jobs do not consume
    /// this field; the glean pipeline carries its own paper audit
    /// profile in `glean_params_template`.
    pub audit_profile: Option<String>,
    /// What [`Self::path`] is.
    pub input: JobInput,
}

impl IngestJob {
    /// An ingest of the source file at `path`: not forced, not held,
    /// under the daemon's default audit profile.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        IngestJob {
            path: path.into(),
            force: false,
            hold_for_metadata: false,
            audit_profile: None,
            input: JobInput::File,
        }
    }
}

/// What the path of an [`IngestJob`] names.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobInput {
    /// The source itself, in a format the pipeline extracts directly.
    File,
    /// A rule file describing how a set of plain-text members makes up
    /// one book.
    Rule {
        /// The scan PDF the rule's text was produced from. When set, the
        /// rule's numbered units are checked against the PDF's pages
        /// and the book is recorded as derived from it.
        source_pdf: Option<PathBuf>,
        /// Accept text that leaves pages of `source_pdf` without a
        /// unit.
        allow_partial: bool,
    },
}

/// One row in the persistent queue.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QueueJob {
    /// UUIDv7 string. A job is located by any fragment of it
    /// (`str::contains`); the leading characters are the millisecond
    /// timestamp, shared by every job queued within the same minute.
    pub id: String,
    /// Library name the job runs against, as registered in the
    /// `LibraryRegistry`.
    pub library: String,
    /// Which side of the library the job works on: `Book` is dispatched
    /// to the ingest pipeline, `Paper` to the glean pipeline.
    pub kind: ItemKind,
    /// Scheduling hint for the worker pull order.
    pub priority: Priority,
    /// What the job does, with everything only that operation reads.
    pub op: JobOp,
    /// Current lifecycle state.
    pub state: JobState,
    /// When the job entered the queue.
    pub queued_at: DateTime<Utc>,
    /// When the worker transitioned this job to `Running`.
    pub started_at: Option<DateTime<Utc>>,
    /// When the worker transitioned this job to `Done`,
    /// `SkippedDuplicate`, `NeedsOcr`, `Failed`, or `Cancelled`.
    pub finished_at: Option<DateTime<Utc>>,
    /// Failure message recorded when `state == Failed`.
    #[serde(default)]
    pub error: Option<String>,
    /// The failure as the control plane classifies it: the same
    /// summary / detail / hint / retryable the error envelope carries
    /// when the same error fails a write RPC. Recorded beside `error`
    /// when `state == Failed` and the daemon recognised the error's
    /// type; `None` for a failure it did not and for every non-failure
    /// state.
    #[serde(default)]
    pub problem: Option<Problem>,
    /// Intake id this job resolved to: the existing catalog row when the
    /// worker skipped the source as a duplicate (`SkippedDuplicate`), or
    /// the `needs_ocr` anchor the worker registered for a scan source
    /// (`NeedsOcr`). Set together with either terminal so `queue list`
    /// can point the operator at the intake without a sha256 lookup.
    #[serde(default)]
    pub merged_into: Option<i64>,
}

impl QueueJob {
    /// The file the job reads, for an operation that reads one.
    pub fn source_path(&self) -> Option<&Path> {
        match &self.op {
            JobOp::Ingest(ingest) => Some(&ingest.path),
        }
    }
}

/// The summary and flattened text recorded on a job queued with the
/// retired metadata hold.
pub const RETIRED_HOLD: &str = "cannot run a job queued with the retired metadata hold";

/// The failure recorded on a job queued with the retired metadata hold:
/// [`RETIRED_HOLD`] with what the hold asked for and what replaces it.
pub fn retired_hold_problem() -> Problem {
    Problem::new(RETIRED_HOLD)
        .detail(
            "The job asked to park the book at the metadata gate, which this build no \
             longer has.",
        )
        .hint(
            "`bookrack queue retry` runs it without the hold. `bookrack dryrun` shows a \
             source's audit verdict without writing to the library. docs/UPGRADE.md covers \
             the change.",
        )
}

/// The summary and flattened text recorded on a job an earlier binary
/// queued for the OCR intake path.
const OCR_PATH_REMOVED: &str = "cannot run a job queued for the removed OCR intake path";

/// The sidecar a document below schema version 8 holds on a job queued
/// for the OCR intake path, read for the one field the failure names.
#[derive(Deserialize)]
struct FlatIntakeOcr {
    from_pdf: PathBuf,
}

/// A queue row as a document below schema version 8 holds it: every
/// field on the row itself, and each field added after version 1
/// defaulting to what the binary before it behaved as.
#[derive(Deserialize)]
struct FlatQueueJob {
    id: String,
    library: String,
    path: PathBuf,
    priority: Priority,
    force: bool,
    #[serde(default)]
    hold_for_metadata: bool,
    #[serde(default)]
    kind: ItemKind,
    #[serde(default)]
    intake_ocr: Option<FlatIntakeOcr>,
    #[serde(default)]
    audit_profile: Option<String>,
    state: JobState,
    queued_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    problem: Option<Problem>,
    #[serde(default)]
    merged_into: Option<i64>,
}

impl FlatQueueJob {
    /// The same job as a current row. A plain ingest keeps every value.
    /// An OCR-intake job loses its sidecar: one that finished or was
    /// cancelled keeps the rest of its record, and one that could still
    /// run — pending, running, failed, or parked for OCR — becomes a
    /// failure naming the removed path, finished at the latest time the
    /// row records.
    fn into_current(self) -> QueueJob {
        let mut job = QueueJob {
            id: self.id,
            library: self.library,
            kind: self.kind,
            priority: self.priority,
            op: JobOp::Ingest(IngestJob {
                path: self.path,
                force: self.force,
                hold_for_metadata: self.hold_for_metadata,
                audit_profile: self.audit_profile,
                input: JobInput::File,
            }),
            state: self.state,
            queued_at: self.queued_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            error: self.error,
            problem: self.problem,
            merged_into: self.merged_into,
        };
        let Some(ocr) = self.intake_ocr else {
            return job;
        };
        if matches!(
            job.state,
            JobState::Done | JobState::SkippedDuplicate | JobState::Cancelled
        ) {
            return job;
        }
        let JobOp::Ingest(ingest) = &job.op;
        let problem = Problem::new(OCR_PATH_REMOVED)
            .detail(format!(
                "The job paired the OCR output {} with the scan {}.",
                ingest.path.display(),
                ocr.from_pdf.display()
            ))
            .hint(
                "Describe the OCR output in a rule file and submit it with \
                 `bookrack ingest --rule`. docs/UPGRADE.md covers the move.",
            );
        job.state = JobState::Failed;
        job.finished_at = job.finished_at.or(job.started_at).or(Some(job.queued_at));
        job.error = Some(OCR_PATH_REMOVED.to_string());
        job.problem = Some(problem);
        job.merged_into = None;
        job
    }
}

/// A whole document below schema version 8.
#[derive(Deserialize)]
struct FlatQueueState {
    schema_version: u32,
    paused: bool,
    jobs: Vec<FlatQueueJob>,
}

/// The one field the version gate reads.
#[derive(Deserialize)]
struct VersionProbe {
    schema_version: u32,
}

/// The `schema_version` a persisted document records, read on its own
/// so a document whose shape this binary cannot parse is still judged
/// by its version first.
pub fn document_version<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(VersionProbe::deserialize(deserializer)?.schema_version)
}

/// Why [`read_document`] did not produce a state.
#[derive(Debug)]
pub enum QueueReadError<E> {
    /// The document records a version above [`QUEUE_SCHEMA_VERSION`].
    TooNew,
    /// The document is not the shape its version describes.
    Malformed(E),
}

/// Read a persisted document that records schema version `found`,
/// in the shape that version wrote.
///
/// A document above [`QUEUE_SCHEMA_VERSION`] is refused. One below
/// version 8 holds flat rows, each converted to a current row. The
/// returned state keeps `found` as its `schema_version`: reading does
/// not relabel a document, the next write does.
pub fn read_document<'de, D>(
    found: u32,
    deserializer: D,
) -> Result<QueueState, QueueReadError<D::Error>>
where
    D: serde::Deserializer<'de>,
{
    if queue_open_decision(found) == QueueOpenDecision::Refuse {
        return Err(QueueReadError::TooNew);
    }
    if found >= FIRST_OP_SCHEMA_VERSION {
        return QueueState::deserialize(deserializer).map_err(QueueReadError::Malformed);
    }
    let flat = FlatQueueState::deserialize(deserializer).map_err(QueueReadError::Malformed)?;
    Ok(QueueState {
        schema_version: flat.schema_version,
        paused: flat.paused,
        jobs: flat
            .jobs
            .into_iter()
            .map(FlatQueueJob::into_current)
            .collect(),
    })
}

/// The first schema version whose rows carry an `op`; every document
/// below it holds flat rows.
// setting: internal -- a fixed point in the document format's history, not a tunable
const FIRST_OP_SCHEMA_VERSION: u32 = 8;

/// The full document persisted to disk.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QueueState {
    /// Schema revision, currently [`QUEUE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// When set, the worker idles instead of pulling pending jobs.
    pub paused: bool,
    /// Every job, in insertion order.
    pub jobs: Vec<QueueJob>,
}

impl Default for QueueState {
    fn default() -> Self {
        QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: Vec::new(),
        }
    }
}

/// Why an id fragment did not locate exactly one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentMiss {
    /// The fragment is empty; every id contains it.
    Empty,
    /// No eligible row's id contains the fragment.
    NoMatch,
    /// More than one eligible row's id contains the fragment.
    Ambiguous { count: usize },
}

/// Index of the one row in `jobs` that `eligible` admits and whose id
/// contains `fragment`, matched anywhere in the id. Rows `eligible`
/// rejects are not candidates: they neither match nor make a match
/// ambiguous.
///
/// A UUIDv7 leads with its millisecond timestamp, so jobs queued within
/// the same minute share their leading characters; any part of the id,
/// including its random tail, is accepted.
pub fn match_id_fragment(
    jobs: &[QueueJob],
    fragment: &str,
    eligible: impl Fn(&QueueJob) -> bool,
) -> Result<usize, FragmentMiss> {
    if fragment.is_empty() {
        return Err(FragmentMiss::Empty);
    }
    let mut candidates = jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| eligible(job) && job.id.contains(fragment))
        .map(|(index, _)| index);
    let Some(first) = candidates.next() else {
        return Err(FragmentMiss::NoMatch);
    };
    match candidates.count() {
        0 => Ok(first),
        rest => Err(FragmentMiss::Ambiguous { count: rest + 1 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(time: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(time)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Read `document` through the two public entry points, the way
    /// both of its on-disk readers do.
    fn read(document: &serde_json::Value) -> Result<QueueState, QueueReadError<serde_json::Error>> {
        let text = document.to_string();
        let found = document_version(&mut serde_json::Deserializer::from_str(&text))
            .expect("the document records a version");
        read_document(found, &mut serde_json::Deserializer::from_str(&text))
    }

    /// The one row of a document at `version` holding `row`.
    fn read_row(version: u32, row: serde_json::Value) -> QueueJob {
        let document = serde_json::json!({
            "schema_version": version,
            "paused": false,
            "jobs": [row],
        });
        let mut state = read(&document).expect("the document loads");
        assert_eq!(state.jobs.len(), 1);
        state.jobs.remove(0)
    }

    /// A flat row as a version-1 binary wrote it: the fields every
    /// version below 8 carries, and none added after the first.
    fn flat_row() -> serde_json::Value {
        serde_json::json!({
            "id": "0",
            "library": "default",
            "path": "/srv/books/example.epub",
            "priority": "normal",
            "force": false,
            "state": "pending",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": null,
            "finished_at": null,
            "error": null
        })
    }

    /// `flat_row` with `extra`'s keys set over it.
    fn flat_row_with(extra: serde_json::Value) -> serde_json::Value {
        let mut row = flat_row();
        for (key, value) in extra.as_object().expect("an object of overrides") {
            row[key] = value.clone();
        }
        row
    }

    fn job_with(id: &str, state: JobState) -> QueueJob {
        QueueJob {
            id: id.to_string(),
            library: "default".to_string(),
            kind: ItemKind::Book,
            priority: Priority::Normal,
            op: JobOp::Ingest(IngestJob::file("/tmp/example.epub")),
            state,
            queued_at: at("2026-01-02T03:04:05Z"),
            started_at: None,
            finished_at: None,
            error: None,
            problem: None,
            merged_into: None,
        }
    }

    /// A recorded failure loads as it was written, text and diagnostic
    /// alike, from a flat row and from a current one.
    #[test]
    fn a_recorded_failure_loads_as_recorded() {
        let error = "ingest: train failed: out of memory";
        let problem = serde_json::json!({
            "summary": "could not train the index",
            "detail": "The trainer ran out of memory.",
            "hint": "Retry with fewer partitions.",
            "retryable": true
        });
        let mut current = serde_json::to_value(job_with("0", JobState::Failed)).unwrap();
        current["error"] = serde_json::json!(error);
        current["problem"] = problem.clone();
        let rows = [
            read_row(
                7,
                flat_row_with(
                    serde_json::json!({ "state": "failed", "error": error, "problem": problem }),
                ),
            ),
            read_row(QUEUE_SCHEMA_VERSION, current),
        ];
        for job in rows {
            assert_eq!(job.error.as_deref(), Some(error));
            let problem = job.problem.expect("the diagnostic loads");
            assert_eq!(problem.summary, "could not train the index");
            assert_eq!(
                problem.data.detail.as_deref(),
                Some("The trainer ran out of memory.")
            );
            assert_eq!(
                problem.data.hint.as_deref(),
                Some("Retry with fewer partitions.")
            );
            assert!(problem.data.retryable);
        }

        let missing = read_row(7, flat_row_with(serde_json::json!({ "state": "failed" })));
        assert_eq!(missing.error, None);
        assert_eq!(missing.problem, None);
    }

    #[test]
    fn every_listed_state_round_trips_through_its_wire_token() {
        for state in JobState::ALL {
            let token = serde_json::Value::String(state.as_wire_str().to_string());
            let read: JobState = serde_json::from_value(token).expect("serde reads the wire token");
            assert_eq!(read, state);
        }
    }

    /// A flat row carrying only the fields version 1 wrote loads as a
    /// plain book ingest: every field added since reads as what the
    /// binary before it behaved as.
    #[test]
    fn a_flat_row_without_the_later_fields_loads_with_their_defaults() {
        let job = read_row(1, flat_row());
        assert_eq!(job.kind, ItemKind::Book);
        assert_eq!(
            job.op,
            JobOp::Ingest(IngestJob::file("/srv/books/example.epub"))
        );
        assert!(job.problem.is_none());
        assert!(job.merged_into.is_none());
    }

    /// Every value a flat row holds reaches the current row, the
    /// operation's fields inside `op` and the rest where they were.
    #[test]
    fn a_flat_row_keeps_every_value_it_holds() {
        for kind in [ItemKind::Book, ItemKind::Paper] {
            let job = read_row(
                7,
                flat_row_with(serde_json::json!({
                    "id": "abc",
                    "library": "beta",
                    "kind": kind,
                    "priority": "high",
                    "force": true,
                    "hold_for_metadata": true,
                    "intake_ocr": null,
                    "audit_profile": "strict",
                    "state": "skipped_duplicate",
                    "started_at": "2026-01-02T03:04:06Z",
                    "finished_at": "2026-01-02T03:04:07Z",
                    "merged_into": 42
                })),
            );
            assert_eq!(
                job,
                QueueJob {
                    id: "abc".to_string(),
                    library: "beta".to_string(),
                    kind,
                    priority: Priority::High,
                    op: JobOp::Ingest(IngestJob {
                        path: "/srv/books/example.epub".into(),
                        force: true,
                        hold_for_metadata: true,
                        audit_profile: Some("strict".to_string()),
                        input: JobInput::File,
                    }),
                    state: JobState::SkippedDuplicate,
                    queued_at: at("2026-01-02T03:04:05Z"),
                    started_at: Some(at("2026-01-02T03:04:06Z")),
                    finished_at: Some(at("2026-01-02T03:04:07Z")),
                    error: None,
                    problem: None,
                    merged_into: Some(42),
                }
            );
        }
    }

    fn flat_ocr_row(extra: serde_json::Value) -> serde_json::Value {
        let mut row = flat_row_with(serde_json::json!({
            "path": "/srv/ocr/example.md",
            "intake_ocr": {
                "from_pdf": "/srv/scans/example.pdf",
                "expected_pages": 42,
                "allow_partial": true
            }
        }));
        for (key, value) in extra.as_object().expect("an object of overrides") {
            row[key] = value.clone();
        }
        row
    }

    /// An OCR-intake job that could still run loads as a failure that
    /// names the removed path and what replaces it, finished at the
    /// latest time the row records, with nothing left that would send
    /// it down the plain ingest path as a pending job.
    #[test]
    fn a_flat_ocr_job_that_could_still_run_loads_as_a_failure() {
        let cases = [
            (
                serde_json::json!({ "state": "pending" }),
                "2026-01-02T03:04:05Z",
            ),
            (
                serde_json::json!({ "state": "running", "started_at": "2026-01-02T03:04:06Z" }),
                "2026-01-02T03:04:06Z",
            ),
            (
                serde_json::json!({
                    "state": "failed",
                    "started_at": "2026-01-02T03:04:06Z",
                    "finished_at": "2026-01-02T03:04:07Z",
                    "error": "ingest: embed backend unreachable",
                    "problem": { "summary": "could not reach the embed backend",
                                 "hint": "Run `bookrack queue retry`.", "retryable": true }
                }),
                "2026-01-02T03:04:07Z",
            ),
            (
                serde_json::json!({ "state": "needs_ocr", "merged_into": 9,
                                    "finished_at": "2026-01-02T03:04:07Z" }),
                "2026-01-02T03:04:07Z",
            ),
        ];
        for (extra, finished) in cases {
            let job = read_row(7, flat_ocr_row(extra.clone()));
            assert_eq!(job.state, JobState::Failed, "{extra}");
            assert_eq!(job.finished_at, Some(at(finished)), "{extra}");
            assert_eq!(job.merged_into, None, "{extra}");
            assert_eq!(
                job.error.as_deref(),
                Some("cannot run a job queued for the removed OCR intake path"),
                "{extra}"
            );
            let problem = job.problem.expect("the failure is classified");
            assert_eq!(
                problem.summary,
                "cannot run a job queued for the removed OCR intake path"
            );
            let detail = problem.data.detail.expect("the detail names the pair");
            assert!(detail.contains("/srv/ocr/example.md"), "{detail}");
            assert!(detail.contains("/srv/scans/example.pdf"), "{detail}");
            let hint = problem.data.hint.expect("the hint names the next step");
            assert!(hint.contains("`bookrack ingest --rule`"), "{hint}");
            assert!(hint.contains("docs/UPGRADE.md"), "{hint}");
            assert!(!hint.contains("retry"), "{hint}");
            assert!(!problem.data.retryable);
        }
    }

    /// An OCR-intake job that finished or was cancelled keeps its
    /// record and loses only the sidecar.
    #[test]
    fn a_flat_ocr_job_that_ended_keeps_its_record() {
        for state in ["done", "skipped_duplicate", "cancelled"] {
            let job = read_row(
                7,
                flat_ocr_row(serde_json::json!({
                    "state": state,
                    "finished_at": "2026-01-02T03:04:07Z",
                    "merged_into": 9
                })),
            );
            assert_eq!(job.state.as_wire_str(), state);
            assert_eq!(job.error, None);
            assert_eq!(job.problem, None);
            assert_eq!(job.merged_into, Some(9));
            assert_eq!(job.finished_at, Some(at("2026-01-02T03:04:07Z")));
            assert_eq!(job.source_path(), Some(Path::new("/srv/ocr/example.md")));
        }
    }

    /// Reading keeps the version the document records, at every
    /// version this binary reads; a document above it is refused before
    /// its shape is looked at.
    #[test]
    fn reading_keeps_the_recorded_version_and_refuses_a_later_one() {
        for version in [1, 7] {
            let document = serde_json::json!({
                "schema_version": version, "paused": true, "jobs": [flat_row()]
            });
            let state = read(&document).expect("an earlier document loads");
            assert_eq!(state.schema_version, version);
            assert!(state.paused);
        }
        let current = serde_json::to_value(QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![job_with("0", JobState::Pending)],
        })
        .unwrap();
        assert_eq!(
            read(&current).expect("a current document loads").jobs,
            vec![job_with("0", JobState::Pending)]
        );

        let later = serde_json::json!({
            "schema_version": QUEUE_SCHEMA_VERSION + 1, "paused": false, "jobs": [{ "shape": "unknown" }]
        });
        assert!(matches!(read(&later), Err(QueueReadError::TooNew)));
    }

    /// Each version is read in its own shape only: a flat row under the
    /// current version is malformed, and so is a current row under an
    /// earlier one.
    #[test]
    fn a_row_in_another_versions_shape_is_malformed() {
        let flat_as_current = serde_json::json!({
            "schema_version": QUEUE_SCHEMA_VERSION, "paused": false, "jobs": [flat_row()]
        });
        assert!(matches!(
            read(&flat_as_current),
            Err(QueueReadError::Malformed(_))
        ));
        let current_as_flat = serde_json::json!({
            "schema_version": 7, "paused": false,
            "jobs": [serde_json::to_value(job_with("0", JobState::Pending)).unwrap()]
        });
        assert!(matches!(
            read(&current_as_flat),
            Err(QueueReadError::Malformed(_))
        ));
    }

    #[test]
    fn job_state_skipped_duplicate_serializes_as_snake_case() {
        // The wire token is what CLI parsers and the MCP session tool
        // key off; keep it stable under `snake_case` so future readers
        // do not silently drop unrecognized states.
        let json = serde_json::to_value(JobState::SkippedDuplicate).expect("serialize");
        assert_eq!(json, "skipped_duplicate");
        let back: JobState = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, JobState::SkippedDuplicate);
    }

    #[test]
    fn job_state_needs_ocr_serializes_as_snake_case() {
        let json = serde_json::to_value(JobState::NeedsOcr).expect("serialize");
        assert_eq!(json, "needs_ocr");
        let back: JobState = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, JobState::NeedsOcr);
    }

    /// A current row survives a write and a read with every field it
    /// can carry set.
    #[test]
    fn a_current_row_round_trips() {
        let job = QueueJob {
            kind: ItemKind::Paper,
            priority: Priority::High,
            op: JobOp::Ingest(IngestJob {
                path: "/tmp/example.md".into(),
                force: true,
                hold_for_metadata: true,
                audit_profile: Some("strict".to_string()),
                input: JobInput::File,
            }),
            merged_into: Some(42),
            ..job_with("0", JobState::SkippedDuplicate)
        };
        let json = serde_json::to_value(&job).expect("serialize");
        assert_eq!(json["kind"], "paper");
        assert_eq!(json["state"], "skipped_duplicate");
        assert_eq!(json["merged_into"], 42);
        assert_eq!(json["op"]["type"], "ingest");
        assert_eq!(json["op"]["path"], "/tmp/example.md");
        assert_eq!(json["op"]["force"], true);
        assert_eq!(json["op"]["hold_for_metadata"], true);
        assert_eq!(json["op"]["audit_profile"], "strict");
        assert_eq!(json["op"]["input"]["type"], "file");
        assert_eq!(read_row(QUEUE_SCHEMA_VERSION, json), job);
    }

    /// A rule input survives a write and a read, with and without the
    /// scan it derives from.
    #[test]
    fn a_rule_input_round_trips() {
        for (source_pdf, allow_partial) in [(None, false), (Some("/tmp/scan.pdf"), true)] {
            let job = QueueJob {
                op: JobOp::Ingest(IngestJob {
                    input: JobInput::Rule {
                        source_pdf: source_pdf.map(PathBuf::from),
                        allow_partial,
                    },
                    ..IngestJob::file("/tmp/material/rule.toml")
                }),
                ..job_with("0", JobState::Pending)
            };
            let json = serde_json::to_value(&job).expect("serialize");
            assert_eq!(json["op"]["input"]["type"], "rule");
            assert_eq!(json["op"]["input"]["allow_partial"], allow_partial);
            assert_eq!(
                json["op"]["input"]["source_pdf"],
                serde_json::json!(source_pdf)
            );
            assert_eq!(read_row(QUEUE_SCHEMA_VERSION, json), job);
        }
    }

    #[test]
    fn a_document_at_or_below_this_version_loads_and_a_later_one_is_refused() {
        assert_eq!(
            queue_open_decision(QUEUE_SCHEMA_VERSION),
            QueueOpenDecision::Match
        );
        assert_eq!(queue_open_decision(0), QueueOpenDecision::Match);
        assert_eq!(
            queue_open_decision(QUEUE_SCHEMA_VERSION - 1),
            QueueOpenDecision::Match
        );
        assert_eq!(
            queue_open_decision(QUEUE_SCHEMA_VERSION + 1),
            QueueOpenDecision::Refuse
        );
        assert_eq!(queue_open_decision(u32::MAX), QueueOpenDecision::Refuse);
    }

    /// Every persisted key, pinned against the version stamp that
    /// describes the document holding it. A field added to any of the
    /// types changes what an older binary sees on disk, which is
    /// exactly what [`QUEUE_SCHEMA_VERSION`] exists to announce — so the
    /// expected sets below and the constant move in the same change,
    /// and a field that arrives without one fails here.
    #[test]
    fn the_persisted_key_set_is_pinned_to_the_schema_version() {
        fn keys_of(value: &serde_json::Value) -> Vec<&str> {
            let mut keys: Vec<&str> = value
                .as_object()
                .expect("a struct serializes as an object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            keys
        }

        assert_eq!(
            QUEUE_SCHEMA_VERSION, 8,
            "the key sets below describe version 8 of the document"
        );

        let job = job_with("0", JobState::Pending);
        let state = serde_json::to_value(QueueState {
            schema_version: QUEUE_SCHEMA_VERSION,
            paused: false,
            jobs: vec![job],
        })
        .expect("serialize");

        assert_eq!(keys_of(&state), ["jobs", "paused", "schema_version"]);
        assert_eq!(
            keys_of(&state["jobs"][0]),
            [
                "error",
                "finished_at",
                "id",
                "kind",
                "library",
                "merged_into",
                "op",
                "priority",
                "problem",
                "queued_at",
                "started_at",
                "state",
            ]
        );
        assert_eq!(state["jobs"][0]["op"]["type"], "ingest");
        assert_eq!(
            keys_of(&state["jobs"][0]["op"]),
            [
                "audit_profile",
                "force",
                "hold_for_metadata",
                "input",
                "path",
                "type",
            ]
        );
        assert_eq!(state["jobs"][0]["op"]["input"]["type"], "file");
        assert_eq!(keys_of(&state["jobs"][0]["op"]["input"]), ["type"]);
        let rule = serde_json::to_value(JobInput::Rule {
            source_pdf: Some("/tmp/scan.pdf".into()),
            allow_partial: true,
        })
        .expect("serialize");
        assert_eq!(rule["type"], "rule");
        assert_eq!(keys_of(&rule), ["allow_partial", "source_pdf", "type"]);
    }

    /// A version 8 row written while the row still carried an
    /// `intake_ocr` key loads as the same job without it.
    #[test]
    fn a_current_row_with_a_leftover_intake_ocr_key_still_loads() {
        let job = job_with("0", JobState::Pending);
        let mut json = serde_json::to_value(&job).expect("serialize");
        json["intake_ocr"] = serde_json::Value::Null;
        assert_eq!(read_row(QUEUE_SCHEMA_VERSION, json), job);
    }

    #[test]
    fn match_id_fragment_finds_a_fragment_anywhere_in_the_id() {
        let jobs = [
            job_with("019a1b2c-3d4e-7f00-8a11-0000aaaa1111", JobState::Pending),
            job_with("029a1b2c-3d4e-7f00-8b22-0000bbbb2222", JobState::Failed),
            job_with("039a1b2c-3d4e-7f00-8c33-0000cccc3333", JobState::Done),
        ];
        let any = |_: &QueueJob| true;
        assert_eq!(match_id_fragment(&jobs, "019a", any), Ok(0));
        assert_eq!(match_id_fragment(&jobs, "8b22", any), Ok(1));
        assert_eq!(match_id_fragment(&jobs, "cccc3333", any), Ok(2));
    }

    #[test]
    fn match_id_fragment_reports_no_match_and_the_ambiguous_count() {
        let jobs = [
            job_with("019a1b2c-3d4e-7f00-8a11-0000aaaa1111", JobState::Pending),
            job_with("019a1b2c-3d4e-7f00-8b22-0000bbbb2222", JobState::Failed),
            job_with("019a1b2c-3d4e-7f00-8c33-0000cccc3333", JobState::Done),
        ];
        let any = |_: &QueueJob| true;
        assert_eq!(
            match_id_fragment(&jobs, "zz", any),
            Err(FragmentMiss::NoMatch)
        );
        assert_eq!(
            match_id_fragment(&jobs, "019a1b2c", any),
            Err(FragmentMiss::Ambiguous { count: 3 })
        );
    }

    /// On a one-row queue an empty fragment would be a unique match,
    /// since every id contains it; the refusal must come first.
    #[test]
    fn match_id_fragment_refuses_an_empty_fragment() {
        let jobs = [job_with("aaaa1111", JobState::Pending)];
        assert_eq!(
            match_id_fragment(&jobs, "", |_| true),
            Err(FragmentMiss::Empty)
        );
    }

    #[test]
    fn match_id_fragment_counts_only_eligible_rows() {
        let jobs = [
            job_with("aaaa1111", JobState::Done),
            job_with("aaaa2222", JobState::Pending),
        ];
        let live = |job: &QueueJob| matches!(job.state, JobState::Pending | JobState::Running);
        assert_eq!(match_id_fragment(&jobs, "aaaa", live), Ok(1));
        assert_eq!(
            match_id_fragment(&jobs, "1111", live),
            Err(FragmentMiss::NoMatch)
        );
    }
}
