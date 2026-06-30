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
use bookrack_runtime::control::jsonrpc::{
    BUSY, CONFIRMATION_REQUIRED, INTERNAL_ERROR, INVALID_LIBRARY, INVALID_PARAMS, INVALID_REQUEST,
    JOB_NOT_FOUND, METHOD_NOT_FOUND, NOT_READY, PARSE_ERROR, PLAN_KIND_MISMATCH,
    PLAN_LIBRARY_MISMATCH, PLAN_NOT_FOUND,
};

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
    RpcUserError { code: i32, message: String },

    /// Daemon is busy or not yet ready to handle the call. A scripted
    /// caller can retry after a backoff.
    #[error("rpc error {code}: {message}")]
    RpcBusy { code: i32, message: String },

    /// Daemon raised an internal error, or returned a JSON-RPC
    /// protocol-layer code (`PARSE_ERROR`, `INVALID_REQUEST`) that
    /// implies the CLI sent something the daemon could not parse.
    /// Treated as a CLI/daemon bug; not retryable.
    #[error("rpc error {code}: {message}")]
    RpcInternal { code: i32, message: String },

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
        }
    }

    /// True for variants whose stdout/stderr renderer has already
    /// drawn the failure surface, so `main`'s reporter must skip
    /// the `bookrack: …` prefix to avoid an extra line of noise.
    pub fn is_self_reported(&self) -> bool {
        matches!(
            self,
            Self::DoctorUnhealthy | Self::IngestPartialFailure { .. }
        )
    }

    /// Classify a JSON-RPC error into the matching CLI variant so the
    /// binary's exit code reflects whether the failure was a user
    /// input mistake (exit 2), a transient busy/not-ready state
    /// (exit 4), or an internal/protocol error (exit 1).
    pub fn from_rpc(code: i32, message: String) -> Self {
        match code {
            METHOD_NOT_FOUND
            | INVALID_PARAMS
            | INVALID_LIBRARY
            | JOB_NOT_FOUND
            | CONFIRMATION_REQUIRED
            | PLAN_NOT_FOUND
            | PLAN_KIND_MISMATCH
            | PLAN_LIBRARY_MISMATCH => Self::RpcUserError { code, message },
            BUSY | NOT_READY => Self::RpcBusy { code, message },
            PARSE_ERROR | INVALID_REQUEST | INTERNAL_ERROR => Self::RpcInternal { code, message },
            _ => Self::RpcInternal { code, message },
        }
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
        if let Some(ControlError::Rpc { code, message, .. }) = cause.downcast_ref::<ControlError>()
        {
            return Some(CliReportCause::Rpc(BookrackCliError::from_rpc(
                *code,
                message.clone(),
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
        ] {
            let err = BookrackCliError::from_rpc(code, "boom".into());
            assert!(
                matches!(err, BookrackCliError::RpcUserError { .. }),
                "code {code} should be RpcUserError"
            );
            assert_eq!(err.exit_code(), 2, "code {code}");
        }
    }

    #[test]
    fn from_rpc_classifies_busy_codes_as_exit_four() {
        for &code in &[BUSY, NOT_READY] {
            let err = BookrackCliError::from_rpc(code, "later".into());
            assert!(matches!(err, BookrackCliError::RpcBusy { .. }));
            assert_eq!(err.exit_code(), 4, "code {code}");
        }
    }

    #[test]
    fn from_rpc_classifies_protocol_and_internal_codes_as_exit_one() {
        for &code in &[PARSE_ERROR, INVALID_REQUEST, INTERNAL_ERROR, -32999] {
            let err = BookrackCliError::from_rpc(code, "bug".into());
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

    #[test]
    fn classify_eyre_returns_none_for_unrelated_errors() {
        let err: eyre::Report = eyre::eyre!("something else");
        assert!(classify_eyre(&err).is_none());
    }
}
