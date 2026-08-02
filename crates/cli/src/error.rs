// SPDX-License-Identifier: Apache-2.0

//! Typed user-facing errors the `bookrack` binary returns instead of
//! reaching for `std::process::exit` in the middle of a subcommand.
//!
//! Each variant carries its own short, operator-targeted message and
//! its own exit code so the top-level reporter in `main` can render
//! a one-line "bookrack: …" prefix and pick the right `ExitCode`
//! without each call site re-rolling its own `eprintln!` /
//! `std::process::exit` pair.
//!
//! Anything that is **not** a [`BookrackCliError`] is treated as an
//! unexpected error and renders through `color-eyre`'s full cause
//! chain so the bug is debuggable.

use std::path::PathBuf;

use bookrack_control_client::ControlError;
use bookrack_core::{Problem, ProblemData};
use bookrack_runtime::control::jsonrpc::{
    BUSY, CONFIRMATION_REQUIRED, INTERNAL_ERROR, INVALID_LIBRARY, INVALID_PARAMS, INVALID_REQUEST,
    JOB_NOT_FOUND, METHOD_NOT_FOUND, NOT_READY, PARSE_ERROR, PLAN_KIND_MISMATCH,
    PLAN_LIBRARY_MISMATCH, PLAN_NOT_FOUND, PLAN_TARGET_DRIFTED,
};
use serde_json::Value;

/// Predictable, operator-facing failures the CLI emits.
#[derive(Debug, thiserror::Error)]
pub enum BookrackCliError {
    /// No daemon is listening at the resolved runtime directory.
    #[error("bookrack daemon not running; start it with: bookrack run")]
    DaemonNotRunning,

    /// The runtime directory or socket exists but the connect failed
    /// for some other reason (permission, codec mismatch, ...).
    #[error("daemon control socket unreachable: {source}")]
    DaemonUnreachable {
        #[source]
        source: ControlError,
    },

    /// `bookrack run` found a lock pointing at a daemon that did not
    /// answer the health probe within the grace window.
    #[error(
        "bookrack session lock at {path} is stale (no live daemon answered within 2s).\nRemove the lock file manually and re-run bookrack: rm {path}",
        path = .path.display()
    )]
    StaleSessionLock { path: PathBuf },

    /// `bookrack run` could not read or interpret the session lock
    /// file. Carries the formatted upstream error verbatim so the
    /// operator sees the underlying cause.
    #[error("{message}")]
    SessionLockUnreadable { message: String },

    /// `bookrack doctor` reported at least one FAIL row. The doctor
    /// renderer already drew its table before the binary returned;
    /// the reporter only needs to set a non-zero exit code.
    #[error("doctor: at least one check failed; see the table above")]
    DoctorUnhealthy,

    /// The invoking shell's explicit library selection
    /// (`--data-dir` / `--library` / `BOOKRACK_DATA_DIR`) disagrees
    /// with the library a running daemon is serving, and the
    /// requested subcommand routes through that daemon. Bail
    /// instead of silently acting on the daemon's library.
    #[error(
        "running daemon serves {running}; refusing to act on {intent}.\nRun `bookrack quit` and start a new session with the desired --library/--data-dir to switch."
    )]
    LibraryMismatch { intent: String, running: String },

    /// Daemon rejected the call as a user-input failure: bad params,
    /// unknown library, unknown job/plan id, missing confirmation
    /// token, or an unknown RPC method (typo or unsupported by this
    /// daemon version).
    #[error("rpc error {code}: {message}")]
    RpcUserError {
        code: i32,
        message: String,
        /// The daemon's `error.data` object, carried through unparsed
        /// so the reporter can render detail and hint. `Display` stays
        /// a self-sufficient single line and ignores it.
        data: Option<Value>,
    },

    /// Daemon is busy or not yet ready to handle the call. A scripted
    /// caller can retry after a backoff.
    #[error("rpc error {code}: {message}")]
    RpcBusy { code: i32, message: String },

    /// Daemon raised an internal error, or returned a JSON-RPC
    /// protocol-layer code (`PARSE_ERROR`, `INVALID_REQUEST`) that
    /// implies the CLI sent something the daemon could not parse.
    /// Treated as a CLI/daemon bug; not retryable.
    #[error("rpc error {code}: {message}")]
    RpcInternal {
        code: i32,
        message: String,
        /// See [`BookrackCliError::RpcUserError`].
        data: Option<Value>,
    },

    /// Awaited a batch of ingest jobs and one or more reached a
    /// non-`Done` terminal state (`Failed` or `Cancelled`). The
    /// per-job summary on stdout has already named the offenders;
    /// the binary only needs to surface a non-zero exit code so
    /// scripts can branch on partial failure.
    #[error("ingest: {failed} failed, {cancelled} cancelled of {total} job(s)")]
    IngestPartialFailure {
        failed: u32,
        cancelled: u32,
        total: u32,
    },

    /// `bookrack rpc call <method> <params>` was handed a params
    /// argument that is not valid JSON. A usage mistake on the
    /// escape-hatch surface, so it shares the exit-2 bucket with the
    /// daemon's own
    /// `-32602 invalid params` rather than falling through to the
    /// exit-1 internal-error path.
    #[error("`{method}`: params are not valid JSON")]
    RpcParamsInvalid { method: String, detail: String },

    /// `bookrack rpc call` was handed a name that cannot be a
    /// control-plane method: every method is namespaced
    /// (`<namespace>.<verb>`). Judged locally, before the call is
    /// sent — the daemon would answer `-32601`, but only after a
    /// round trip and without being able to say what shape was
    /// expected. Shares the exit-2 user-error bucket.
    #[error("`{method}` is not a control-plane method name")]
    RpcMethodNotNamespaced { method: String },

    /// A locally-resolved command (one that acts on the registry or a
    /// data root without a daemon, e.g. `libraries default <name>`
    /// naming a library the registry does not define) rejected the
    /// operator's input. Shares the exit-2 user-error bucket; carries
    /// the underlying message verbatim so the reason is self-contained.
    #[error("{message}")]
    LocalUserError { message: String },

    /// A destructive action needed a confirmation and stdin could not
    /// carry one — the stream ended before any byte arrived. Distinct
    /// from a decline, which is an answer and exits 0: nothing was
    /// decided here, so the caller must not read the run as either a
    /// completed action or a considered refusal. Shares the exit-2
    /// user-error bucket.
    #[error("{action}: {reason}. {hint}")]
    ConfirmationUnanswerable {
        action: String,
        reason: String,
        hint: String,
    },

    /// The daemon refused to start because an external backend it
    /// needs is unusable — the check runs before any library is
    /// opened, so nothing was half-started. Operator input, not a
    /// bug: exit 2, and the reporter draws the three parts.
    #[error("{}", .problem.summary)]
    PreflightRefused { problem: Problem },

    /// `libraries detect <path>` determined the path is not a confirmed
    /// or probable bookrack data root — a plain not-a-library verdict or
    /// an unreadable manifest. The renderer already printed the verdict;
    /// the reporter only needs a non-zero (exit 1) code.
    #[error("detect: {} is not a bookrack data root", .0.display())]
    DetectNegative(PathBuf),
}

impl BookrackCliError {
    /// Exit code the binary returns for this failure. See
    /// `docs/control-plane.md` for the full exit-code table.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::DaemonNotRunning | Self::DaemonUnreachable { .. } => 2,
            Self::StaleSessionLock { .. } => 3,
            Self::SessionLockUnreadable { .. } => 1,
            Self::DoctorUnhealthy => 1,
            Self::LibraryMismatch { .. } => 2,
            Self::RpcUserError { .. } => 2,
            Self::RpcBusy { .. } => 4,
            Self::RpcInternal { .. } => 1,
            Self::IngestPartialFailure { .. } => 5,
            Self::RpcParamsInvalid { .. } | Self::RpcMethodNotNamespaced { .. } => 2,
            Self::LocalUserError { .. } => 2,
            Self::ConfirmationUnanswerable { .. } => 2,
            Self::PreflightRefused { .. } => 2,
            Self::DetectNegative(_) => 1,
        }
    }

    /// True for variants whose stdout/stderr renderer has already
    /// drawn the failure surface, so `main`'s reporter must skip
    /// the `bookrack: …` prefix to avoid an extra line of noise.
    pub fn is_self_reported(&self) -> bool {
        matches!(
            self,
            Self::DoctorUnhealthy | Self::IngestPartialFailure { .. } | Self::DetectNegative(_)
        )
    }

    /// Classify a JSON-RPC error into the matching CLI variant so the
    /// binary's exit code reflects whether the failure was a user
    /// input mistake (exit 2), a transient busy/not-ready state
    /// (exit 4), or an internal/protocol error (exit 1).
    pub fn from_rpc(code: i32, message: String, data: Option<Value>) -> Self {
        match code {
            METHOD_NOT_FOUND
            | INVALID_PARAMS
            | INVALID_LIBRARY
            | JOB_NOT_FOUND
            | CONFIRMATION_REQUIRED
            | PLAN_NOT_FOUND
            | PLAN_KIND_MISMATCH
            | PLAN_LIBRARY_MISMATCH
            | PLAN_TARGET_DRIFTED => Self::RpcUserError {
                code,
                message,
                data,
            },
            BUSY | NOT_READY => Self::RpcBusy { code, message },
            PARSE_ERROR | INVALID_REQUEST | INTERNAL_ERROR => Self::RpcInternal {
                code,
                message,
                data,
            },
            _ => Self::RpcInternal {
                code,
                message,
                data,
            },
        }
    }

    /// The detail / hint / retryable triple the daemon attached to this
    /// failure, if it attached one.
    ///
    /// Read back as a whole [`ProblemData`] rather than key by key, so
    /// the CLI cannot drift from the shape the control plane emits. A
    /// `data` slot that does not parse is treated as absent: an
    /// unrenderable extra is not worth failing the report over.
    pub fn problem_data(&self) -> Option<ProblemData> {
        let data = match self {
            Self::RpcUserError { data, .. } | Self::RpcInternal { data, .. } => data.as_ref()?,
            Self::PreflightRefused { problem } => return Some(problem.data.clone()),
            Self::RpcParamsInvalid { detail, .. } => {
                return Some(ProblemData {
                    detail: Some(detail.clone()),
                    hint: Some(
                        "Pass a JSON object, e.g. `{}`, or omit the argument entirely to \
                         send `null`."
                            .to_string(),
                    ),
                    retryable: false,
                });
            }
            Self::RpcMethodNotNamespaced { .. } => {
                return Some(ProblemData {
                    detail: Some(
                        "A control-plane method name carries a namespace: \
                         `<namespace>.<verb>`, for example `library.show_book`."
                            .to_string(),
                    ),
                    hint: Some(
                        "Run `bookrack rpc list` to see the method names the running \
                         daemon answers."
                            .to_string(),
                    ),
                    retryable: false,
                });
            }
            _ => return None,
        };
        serde_json::from_value(data.clone()).ok()
    }
}

/// Outcome of walking an `eyre::Report` chain for a known error type.
/// `main`'s reporter inspects this so that `.context("...")` wrappers
/// around an RPC call do not collapse the cause into the fallback
/// exit code.
pub enum CliReportCause<'a> {
    /// A typed `BookrackCliError` was found in the chain; use it
    /// verbatim.
    Cli(&'a BookrackCliError),
    /// A `ControlError::Rpc` from the control client was found in the
    /// chain; this owned variant carries the classification.
    Rpc(BookrackCliError),
}

impl CliReportCause<'_> {
    /// Borrow the underlying `BookrackCliError` regardless of whether
    /// it was found in the chain or freshly classified.
    pub fn as_cli(&self) -> &BookrackCliError {
        match self {
            Self::Cli(e) => e,
            Self::Rpc(e) => e,
        }
    }
}

/// Walk an `eyre::Report` chain for a typed CLI error or an unwrapped
/// JSON-RPC error from the control client.
pub fn classify_eyre(err: &eyre::Report) -> Option<CliReportCause<'_>> {
    for cause in err.chain() {
        if let Some(cli_err) = cause.downcast_ref::<BookrackCliError>() {
            return Some(CliReportCause::Cli(cli_err));
        }
        if let Some(ControlError::Rpc {
            code,
            message,
            data,
        }) = cause.downcast_ref::<ControlError>()
        {
            return Some(CliReportCause::Rpc(BookrackCliError::from_rpc(
                *code,
                message.clone(),
                data.clone(),
            )));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_documented_values() {
        assert_eq!(BookrackCliError::DaemonNotRunning.exit_code(), 2);
        assert_eq!(
            BookrackCliError::StaleSessionLock {
                path: PathBuf::from("/x")
            }
            .exit_code(),
            3
        );
        assert_eq!(BookrackCliError::DoctorUnhealthy.exit_code(), 1);
        assert_eq!(
            BookrackCliError::LibraryMismatch {
                intent: "library x".into(),
                running: "library y".into(),
            }
            .exit_code(),
            2
        );
        assert_eq!(
            BookrackCliError::LocalUserError {
                message: "x".into()
            }
            .exit_code(),
            2
        );
    }

    /// A confirmation that could not be asked is a user error, not a
    /// decline: it must exit 2 and it must tell the operator how to
    /// proceed without a terminal, because the callers that hit it are
    /// scripts whose only signal is the exit code.
    #[test]
    fn confirmation_unanswerable_is_exit_two_and_names_the_escape_hatch() {
        let err = BookrackCliError::ConfirmationUnanswerable {
            action: "libraries remove --purge".into(),
            reason: crate::render::confirm::NoAnswer::EndOfStream.reason(),
            hint: "re-run with --yes to confirm".into(),
        };
        assert_eq!(err.exit_code(), 2);
        assert!(
            !err.is_self_reported(),
            "the reporter must print this one; no call site renders it"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("libraries remove --purge"),
            "the message must name the action: {rendered}"
        );
        assert!(
            rendered.contains("end of file"),
            "the message must say why no answer arrived: {rendered}"
        );
        assert!(
            rendered.contains("--yes"),
            "the message must name the escape hatch: {rendered}"
        );
    }

    #[test]
    fn library_mismatch_message_points_at_quit_and_names_both_sides() {
        let s = BookrackCliError::LibraryMismatch {
            intent: "/asked".into(),
            running: "/served (library a)".into(),
        }
        .to_string();
        assert!(s.contains("/asked"));
        assert!(s.contains("/served (library a)"));
        assert!(s.contains("bookrack quit"));
    }

    #[test]
    fn daemon_not_running_message_is_actionable() {
        let s = BookrackCliError::DaemonNotRunning.to_string();
        assert!(s.contains("bookrack run"));
    }

    #[test]
    fn doctor_unhealthy_is_self_reported() {
        assert!(BookrackCliError::DoctorUnhealthy.is_self_reported());
        assert!(!BookrackCliError::DaemonNotRunning.is_self_reported());
    }

    #[test]
    fn ingest_partial_failure_uses_exit_five_and_is_self_reported() {
        let err = BookrackCliError::IngestPartialFailure {
            failed: 1,
            cancelled: 0,
            total: 3,
        };
        assert_eq!(err.exit_code(), 5);
        assert!(err.is_self_reported());
        let s = err.to_string();
        assert!(s.contains("1 failed"));
        assert!(s.contains("0 cancelled"));
        assert!(s.contains("3 job"));
    }

    #[test]
    fn rpc_params_invalid_uses_exit_two_and_teaches_the_operator() {
        let err = BookrackCliError::RpcParamsInvalid {
            method: "library.stats".into(),
            detail: "invalid number at line 1 column 2".into(),
        };
        assert_eq!(err.exit_code(), 2);
        assert!(!err.is_self_reported());
        // The summary states the fact and nothing else; the evidence
        // and the next step live in their own fields, so a terse
        // renderer that drops them still prints a true line.
        let s = err.to_string();
        assert!(s.contains("library.stats"), "{s}");
        assert!(!s.contains("omit"), "the summary gives no advice: {s}");
        assert!(
            !s.contains("invalid number at line 1 column 2"),
            "the summary carries no serde evidence: {s}"
        );

        let data = err.problem_data().expect("a usage failure is explained");
        assert_eq!(
            data.detail.as_deref(),
            Some("invalid number at line 1 column 2"),
        );
        let hint = data.hint.expect("a usage failure says what to do next");
        assert!(hint.contains("{}"), "{hint}");
        assert!(hint.contains("omit"), "{hint}");
        assert!(!data.retryable, "the same bad JSON fails again");
    }

    #[test]
    fn rpc_method_not_namespaced_uses_exit_two_and_points_at_the_method_list() {
        let err = BookrackCliError::RpcMethodNotNamespaced {
            method: "info".into(),
        };
        assert_eq!(err.exit_code(), 2);
        assert!(!err.is_self_reported());
        let s = err.to_string();
        assert!(s.contains("info"), "{s}");
        assert!(!s.contains("rpc list"), "the summary gives no advice: {s}");

        let data = err.problem_data().expect("a usage failure is explained");
        assert!(
            data.detail
                .as_deref()
                .is_some_and(|d| d.contains("namespace")),
            "{data:?}"
        );
        let hint = data.hint.expect("a usage failure says what to do next");
        assert!(hint.contains("rpc list"), "{hint}");
        assert!(!data.retryable, "the same name fails again");
    }

    #[test]
    fn from_rpc_classifies_user_codes_as_exit_two() {
        for &code in &[
            METHOD_NOT_FOUND,
            INVALID_PARAMS,
            INVALID_LIBRARY,
            JOB_NOT_FOUND,
            CONFIRMATION_REQUIRED,
            PLAN_NOT_FOUND,
            PLAN_KIND_MISMATCH,
            PLAN_LIBRARY_MISMATCH,
            PLAN_TARGET_DRIFTED,
        ] {
            let err = BookrackCliError::from_rpc(code, "boom".into(), None);
            assert!(
                matches!(err, BookrackCliError::RpcUserError { .. }),
                "code {code} should be RpcUserError"
            );
            assert_eq!(err.exit_code(), 2, "code {code}");
        }
    }

    /// `docs/control-plane.md` promises the plan block `-32013..-32016`
    /// exits 2. `from_rpc` ends in a `_` arm, so a code added inside
    /// that block and never registered would exit 1 in silence. Walk
    /// the range rather than the constants: the range is what the
    /// document commits to, and a constant list would be updated by the
    /// same edit that forgets the arm.
    #[test]
    fn every_code_in_the_documented_plan_block_exits_two() {
        for code in -32016..=-32013 {
            let err = BookrackCliError::from_rpc(code, "boom".into(), None);
            assert_eq!(
                err.exit_code(),
                2,
                "code {code} is inside the documented plan block"
            );
        }
    }

    #[test]
    fn from_rpc_classifies_busy_codes_as_exit_four() {
        for &code in &[BUSY, NOT_READY] {
            let err = BookrackCliError::from_rpc(code, "later".into(), None);
            assert!(matches!(err, BookrackCliError::RpcBusy { .. }));
            assert_eq!(err.exit_code(), 4, "code {code}");
        }
    }

    #[test]
    fn from_rpc_classifies_protocol_and_internal_codes_as_exit_one() {
        for &code in &[PARSE_ERROR, INVALID_REQUEST, INTERNAL_ERROR, -32999] {
            let err = BookrackCliError::from_rpc(code, "bug".into(), None);
            assert!(matches!(err, BookrackCliError::RpcInternal { .. }));
            assert_eq!(err.exit_code(), 1, "code {code}");
        }
    }

    #[test]
    fn classify_eyre_finds_typed_cli_error_through_context_wrappers() {
        let err: eyre::Report = eyre::Report::from(BookrackCliError::DaemonNotRunning)
            .wrap_err("running `library.show_book`")
            .wrap_err("first context");
        let cause = classify_eyre(&err).expect("typed CLI error must be found");
        assert!(matches!(cause.as_cli(), BookrackCliError::DaemonNotRunning));
    }

    #[test]
    fn classify_eyre_classifies_wrapped_rpc_error() {
        let rpc = ControlError::Rpc {
            code: INVALID_PARAMS,
            message: "bad arg `n`".into(),
            data: None,
        };
        let err: eyre::Report = eyre::Report::from(rpc).wrap_err("logs.tail rpc");
        let cause = classify_eyre(&err).expect("wrapped RPC must classify");
        let cli_err = cause.as_cli();
        assert!(matches!(cli_err, BookrackCliError::RpcUserError { .. }));
        assert_eq!(cli_err.exit_code(), 2);
    }

    /// The daemon attaches detail and hint to `error.data`; the CLI
    /// has to still be holding them when the reporter runs. Before
    /// this, `classify_eyre` destructured `ControlError::Rpc` with a
    /// `..` that dropped the slot on the floor.
    #[test]
    fn classify_eyre_keeps_the_data_slot_from_the_rpc_error() {
        let rpc = ControlError::Rpc {
            code: INTERNAL_ERROR,
            message: "cannot embed: the model \"m\" is not available".into(),
            data: Some(serde_json::json!({
                "detail": "Ollama answered HTTP 404: model not found.",
                "hint": "Pull it first: ollama pull m.",
                "retryable": false,
            })),
        };
        let err: eyre::Report = eyre::Report::from(rpc).wrap_err("library.search rpc");
        let cause = classify_eyre(&err).expect("wrapped RPC must classify");
        let problem = cause
            .as_cli()
            .problem_data()
            .expect("the data slot must survive classification");
        assert_eq!(
            problem.hint.as_deref(),
            Some("Pull it first: ollama pull m.")
        );
        assert!(!problem.retryable);
    }

    /// A daemon that sends no `data`, or sends something the CLI
    /// cannot parse, must not turn into a reporting failure — the
    /// single-line report is still correct.
    #[test]
    fn an_absent_or_unparseable_data_slot_reads_as_no_problem_data() {
        let none = BookrackCliError::from_rpc(INVALID_PARAMS, "bad arg".into(), None);
        assert!(none.problem_data().is_none());

        let junk = BookrackCliError::from_rpc(
            INVALID_PARAMS,
            "bad arg".into(),
            Some(serde_json::json!(["not", "an", "object"])),
        );
        assert!(junk.problem_data().is_none());
    }

    #[test]
    fn classify_eyre_returns_none_for_unrelated_errors() {
        let err: eyre::Report = eyre::eyre!("something else");
        assert!(classify_eyre(&err).is_none());
    }
}
