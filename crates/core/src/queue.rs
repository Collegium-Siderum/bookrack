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

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ItemKind;

/// Schema version embedded in the persisted document. Bumped whenever
/// any field shape, enum variant, or invariant changes.
// setting: internal -- a version stamp; docs/UPGRADE.md's runbook governs a bump
pub const QUEUE_SCHEMA_VERSION: u32 = 6;

/// What a reader does with a persisted document, given the
/// `schema_version` it carries.
///
/// The two directions are not symmetric. A document an earlier binary
/// wrote is loaded: every field added since carries `#[serde(default)]`,
/// so it reads as the shape that binary behaved as. A document a later
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
/// as a `needs_ocr` intake anchor for a later OCR pass but wrote no
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

/// Side data carried by an OCR-intake job. When set on a [`QueueJob`],
/// the worker treats the job as a derived-source intake: the job's
/// `path` is the OCR markdown product, and the fields below name the
/// scan PDF the product was produced from along with the runtime knobs
/// the OCR ingest path needs. Defaults are the same as the standalone
/// runner's: no expected-page override, partial coverage rejected.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IntakeOcrInfo {
    /// Path to the scan PDF the OCR product was produced from.
    pub from_pdf: PathBuf,
    /// Override the expected page count rather than reading it from
    /// the source PDF's `/Pages`.
    #[serde(default)]
    pub expected_pages: Option<u32>,
    /// Accept a partial OCR product. Missing sheets surface in the OCR
    /// intake's `partial_pages` field rather than being silently
    /// treated as blank.
    #[serde(default)]
    pub allow_partial: bool,
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
    /// Source file to ingest. Resolved when the job was enqueued; the
    /// worker does not re-resolve relative paths.
    pub path: PathBuf,
    /// Scheduling hint for the worker pull order.
    pub priority: Priority,
    /// Force a fresh ingest even when the source's noop-if-up-to-date
    /// check would otherwise short-circuit.
    pub force: bool,
    /// Park the book at STRUCTURE when the audit verdict is
    /// `needs_work`, skipping CHUNK and EMBED until a curator
    /// resumes it. Off by default; a v2 queue document (no
    /// `hold_for_metadata` field) loads with the flag unset, matching
    /// the previous worker behaviour.
    #[serde(default)]
    pub hold_for_metadata: bool,
    /// Which pipeline owns this job: `Book` is dispatched to the ingest
    /// pipeline, `Paper` to the glean pipeline. Defaults to `Book`, so
    /// a v1 queue document (no `kind` field) loads as a book queue.
    #[serde(default)]
    pub kind: ItemKind,
    /// When set, the job is an OCR-intake ingest. The worker routes it
    /// to the OCR ingest path with `path` as the OCR markdown product
    /// and the nested `from_pdf` as the source PDF anchor. A v3 queue
    /// document (no `intake_ocr` field) loads with this `None`, so an
    /// upgrade reads the previous shape as a plain book ingest.
    #[serde(default)]
    pub intake_ocr: Option<IntakeOcrInfo>,
    /// Book-side audit profile override for this job. When `None`, the
    /// worker uses the daemon's default profile baked into the queue
    /// params template at startup. When `Some(name)`, the worker
    /// reloads the named profile per job and substitutes it into the
    /// ingest params before dispatch. Paper-side jobs do not consume
    /// this field; the glean pipeline carries its own paper audit
    /// profile in `glean_params_template`. A v4 queue document (no
    /// `audit_profile` field) loads with this `None`, preserving the
    /// previous default-profile behaviour.
    #[serde(default)]
    pub audit_profile: Option<String>,
    /// Current lifecycle state.
    pub state: JobState,
    /// When the job entered the queue.
    pub queued_at: DateTime<Utc>,
    /// When the worker transitioned this job to `Running`.
    pub started_at: Option<DateTime<Utc>>,
    /// When the worker transitioned this job to `Done`,
    /// `SkippedDuplicate`, `NeedsOcr`, `Failed`, or `Cancelled`.
    pub finished_at: Option<DateTime<Utc>>,
    /// Failure message recorded when `state == Failed`. Redacted as
    /// it loads ([`crate::redact_source_paths`]), so a message recorded
    /// before errors were redacted at their source leaves the process
    /// as clean as a new one.
    #[serde(default, deserialize_with = "deserialize_redacted")]
    pub error: Option<String>,
    /// Intake id this job resolved to: the existing catalog row when the
    /// worker skipped the source as a duplicate (`SkippedDuplicate`), or
    /// the `needs_ocr` anchor the worker registered for a scan source
    /// (`NeedsOcr`). Set together with either terminal so `queue list`
    /// can point the operator at the intake without a sha256 lookup. A
    /// v4 queue document (no `merged_into` field) loads with this `None`,
    /// matching the previous single-`done` terminal.
    #[serde(default)]
    pub merged_into: Option<i64>,
}

/// An optional string, redacted of build-machine source prefixes.
fn deserialize_redacted<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = Option::<String>::deserialize(deserializer)?;
    Ok(text.map(|t| match crate::redact_source_paths(&t) {
        std::borrow::Cow::Borrowed(_) => t,
        std::borrow::Cow::Owned(redacted) => redacted,
    }))
}

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

    /// A failure recorded before errors were redacted at their source
    /// is redacted as it loads: the failure text reaches `queue.list`,
    /// the queue snapshot, and the next save only through this field.
    #[test]
    fn a_recorded_failure_loads_without_build_prefixes() {
        let failed = r#"{
            "id": "0",
            "library": "default",
            "path": "/srv/books/example.epub",
            "kind": "book",
            "priority": "normal",
            "force": false,
            "state": "failed",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": "2026-01-02T03:04:06Z",
            "finished_at": "2026-01-02T03:04:07Z",
            "error": "ingest: train failed, /disk/cache/cargo/registry/src/index.crates.io-0123456789abcdef/dep-1.0.0/src/train.rs:10:4: train failed"
        }"#;
        let job: QueueJob = serde_json::from_str(failed).expect("deserialize");
        assert_eq!(
            job.error.as_deref(),
            Some("ingest: train failed, dep-1.0.0/src/train.rs:10:4: train failed")
        );
        assert_eq!(
            job.path,
            std::path::PathBuf::from("/srv/books/example.epub")
        );

        let missing = failed.replace(
            r#""error": "ingest: train failed, /disk/cache/cargo/registry/src/index.crates.io-0123456789abcdef/dep-1.0.0/src/train.rs:10:4: train failed""#,
            r#""error": null"#,
        );
        let job: QueueJob = serde_json::from_str(&missing).expect("deserialize null");
        assert_eq!(job.error, None);
    }

    #[test]
    fn every_listed_state_round_trips_through_its_wire_token() {
        for state in JobState::ALL {
            let token = serde_json::Value::String(state.as_wire_str().to_string());
            let read: JobState = serde_json::from_value(token).expect("serde reads the wire token");
            assert_eq!(read, state);
        }
    }

    #[test]
    fn queue_job_without_kind_loads_as_book() {
        // A v1-shaped document persisted before [`QUEUE_SCHEMA_VERSION`]
        // grew the `kind` field must still deserialize cleanly, with
        // every job defaulting to the book pipeline.
        let v1 = r#"{
            "id": "0",
            "library": "default",
            "path": "/tmp/example.epub",
            "priority": "normal",
            "force": false,
            "state": "pending",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": null,
            "finished_at": null,
            "error": null
        }"#;
        let job: QueueJob = serde_json::from_str(v1).expect("deserialize v1");
        assert_eq!(job.kind, ItemKind::Book);
    }

    #[test]
    fn queue_job_without_hold_for_metadata_loads_as_false() {
        // A v2-shaped document persisted before the field was added
        // must still deserialize cleanly, with the flag defaulting to
        // off so the worker preserves the previous behaviour.
        let v2 = r#"{
            "id": "0",
            "library": "default",
            "path": "/tmp/example.epub",
            "kind": "book",
            "priority": "normal",
            "force": false,
            "state": "pending",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": null,
            "finished_at": null,
            "error": null
        }"#;
        let job: QueueJob = serde_json::from_str(v2).expect("deserialize v2");
        assert!(!job.hold_for_metadata);
    }

    #[test]
    fn queue_job_without_intake_ocr_loads_as_none() {
        // A v3-shaped document persisted before the field was added
        // must still deserialize cleanly, with the side data defaulting
        // to `None` so the worker treats the job as a plain book ingest.
        let v3 = r#"{
            "id": "0",
            "library": "default",
            "path": "/tmp/example.epub",
            "kind": "book",
            "priority": "normal",
            "force": false,
            "hold_for_metadata": false,
            "state": "pending",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": null,
            "finished_at": null,
            "error": null
        }"#;
        let job: QueueJob = serde_json::from_str(v3).expect("deserialize v3");
        assert!(job.intake_ocr.is_none());
    }

    #[test]
    fn queue_job_without_audit_profile_loads_as_none() {
        // A v4-shaped document persisted before the field was added
        // must still deserialize cleanly, with the per-job override
        // defaulting to `None` so the worker falls back to the daemon
        // params template's profile.
        let v4 = r#"{
            "id": "0",
            "library": "default",
            "path": "/tmp/example.epub",
            "kind": "book",
            "priority": "normal",
            "force": false,
            "hold_for_metadata": false,
            "intake_ocr": null,
            "state": "pending",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": null,
            "finished_at": null,
            "error": null
        }"#;
        let job: QueueJob = serde_json::from_str(v4).expect("deserialize v4");
        assert!(job.audit_profile.is_none());
    }

    #[test]
    fn queue_job_without_merged_into_loads_as_none() {
        // A v4-shaped document persisted before the field was added
        // must still deserialize cleanly, with the dedup back-pointer
        // defaulting to `None` so a job without it renders identically
        // to the previous single-`done` terminal.
        let v4 = r#"{
            "id": "0",
            "library": "default",
            "path": "/tmp/example.epub",
            "kind": "book",
            "priority": "normal",
            "force": false,
            "hold_for_metadata": false,
            "intake_ocr": null,
            "audit_profile": null,
            "state": "done",
            "queued_at": "2026-01-02T03:04:05Z",
            "started_at": null,
            "finished_at": null,
            "error": null
        }"#;
        let job: QueueJob = serde_json::from_str(v4).expect("deserialize v4");
        assert!(job.merged_into.is_none());
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

    #[test]
    fn queue_job_round_trips_merged_into() {
        let job = QueueJob {
            id: "0".to_string(),
            library: "default".to_string(),
            path: "/tmp/example.epub".into(),
            kind: ItemKind::Book,
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            intake_ocr: None,
            audit_profile: None,
            state: JobState::SkippedDuplicate,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: Some(42),
        };
        let json = serde_json::to_value(&job).expect("serialize");
        assert_eq!(json["state"], "skipped_duplicate");
        assert_eq!(json["merged_into"], 42);
        let back: QueueJob = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, job);
    }

    #[test]
    fn queue_job_round_trips_intake_ocr() {
        let job = QueueJob {
            id: "0".to_string(),
            library: "default".to_string(),
            path: "/tmp/example.md".into(),
            kind: ItemKind::Book,
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            intake_ocr: Some(IntakeOcrInfo {
                from_pdf: "/tmp/scan.pdf".into(),
                expected_pages: Some(42),
                allow_partial: true,
            }),
            audit_profile: None,
            state: JobState::Pending,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
        };
        let json = serde_json::to_value(&job).expect("serialize");
        assert_eq!(json["intake_ocr"]["from_pdf"], "/tmp/scan.pdf");
        assert_eq!(json["intake_ocr"]["expected_pages"], 42);
        assert_eq!(json["intake_ocr"]["allow_partial"], true);
        let back: QueueJob = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, job);
    }

    #[test]
    fn queue_job_round_trips_kind() {
        let job = QueueJob {
            id: "0".to_string(),
            library: "default".to_string(),
            path: "/tmp/example.pdf".into(),
            kind: ItemKind::Paper,
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            intake_ocr: None,
            audit_profile: None,
            state: JobState::Pending,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
        };
        let json = serde_json::to_value(&job).expect("serialize");
        assert_eq!(json["kind"], "paper");
        let back: QueueJob = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.kind, ItemKind::Paper);
        assert_eq!(back, job);
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
    /// three types changes what an older binary sees on disk, which is
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
            QUEUE_SCHEMA_VERSION, 6,
            "the key sets below describe version 6 of the document"
        );

        let job = QueueJob {
            id: "0".to_string(),
            library: "default".to_string(),
            path: "/tmp/example.md".into(),
            kind: ItemKind::Book,
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            intake_ocr: Some(IntakeOcrInfo {
                from_pdf: "/tmp/scan.pdf".into(),
                expected_pages: Some(42),
                allow_partial: true,
            }),
            audit_profile: Some("strict".to_string()),
            state: JobState::Pending,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
        };
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
                "audit_profile",
                "error",
                "finished_at",
                "force",
                "hold_for_metadata",
                "id",
                "intake_ocr",
                "kind",
                "library",
                "merged_into",
                "path",
                "priority",
                "queued_at",
                "started_at",
                "state",
            ]
        );
        assert_eq!(
            keys_of(&state["jobs"][0]["intake_ocr"]),
            ["allow_partial", "expected_pages", "from_pdf"]
        );
    }

    #[test]
    fn queue_job_round_trips_audit_profile() {
        let job = QueueJob {
            id: "0".to_string(),
            library: "default".to_string(),
            path: "/tmp/example.epub".into(),
            kind: ItemKind::Book,
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            intake_ocr: None,
            audit_profile: Some("strict".to_string()),
            state: JobState::Pending,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
        };
        let json = serde_json::to_value(&job).expect("serialize");
        assert_eq!(json["audit_profile"], "strict");
        let back: QueueJob = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.audit_profile.as_deref(), Some("strict"));
        assert_eq!(back, job);
    }

    fn job_with(id: &str, state: JobState) -> QueueJob {
        QueueJob {
            id: id.to_string(),
            library: "default".to_string(),
            path: "/tmp/example.epub".into(),
            kind: ItemKind::Book,
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            intake_ocr: None,
            audit_profile: None,
            state,
            queued_at: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
        }
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
