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
}

impl BookrackCliError {
    /// Exit code the binary returns for this failure.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::DaemonNotRunning | Self::DaemonUnreachable { .. } => 2,
            Self::StaleSessionLock { .. } => 3,
            Self::SessionLockUnreadable { .. } => 1,
            Self::DoctorUnhealthy => 1,
            Self::LibraryMismatch { .. } => 2,
        }
    }

    /// True for `DoctorUnhealthy`, which lets `main` skip its own
    /// `bookrack: …` prefix because the doctor renderer has already
    /// drawn the failure rows.
    pub fn is_self_reported(&self) -> bool {
        matches!(self, Self::DoctorUnhealthy)
    }
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
}
