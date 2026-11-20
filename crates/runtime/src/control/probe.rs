// SPDX-License-Identifier: Apache-2.0

//! Control-plane health probe.
//!
//! Given a snapshot of a session lock file, decides whether the
//! recorded daemon is reachable, gone, or never knew its control-plane
//! address. Callers feed the verdict into the second-launch decision:
//!
//! - [`HealthProbe::Healthy`] — the daemon answered `daemon.version`
//!   inside the timeout; the caller can hand its work over or surface
//!   the recorded address and exit cleanly.
//! - [`HealthProbe::Stale`] — the lock is held and names a
//!   control-plane socket, but nothing answered there; the caller
//!   reports those two facts and points the operator at the process
//!   holding the lock.
//! - [`HealthProbe::Unprobeable`] — the lock did not name a
//!   control-plane socket; the caller falls back to the underlying
//!   acquire error.
//! - [`HealthProbe::InvalidEndpoint`] — the lock names an endpoint a
//!   daemon owning it does not bind; nothing is dialed.
//!
//! [`classify_session_lock`] is the whole reading of a session lock:
//! whether it is there, whether a process holds it, and what the probe
//! says of the daemon it records.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_control_client::{ControlSocket, connect};
use bookrack_session::{LockEndpointState, LockInfo, check_lock_endpoint};
use serde_json::Value;

/// How long a caller waits on each step of probing a held session lock.
// setting: internal -- liveness bound for probing a held session lock on second launch, status, or offline repair
pub const SESSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Verdict of a single probe attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthProbe {
    /// Daemon answered `daemon.version` inside the timeout. Carries
    /// the recorded pid and the control-plane socket path so the
    /// caller can surface either to the operator.
    Healthy(u32, PathBuf),
    /// The lock named a control-plane socket but no live daemon
    /// answered inside the timeout (the socket is not there, connect
    /// failed, or `daemon.version` did not return).
    Stale,
    /// The lock did not name a control-plane socket — typically a
    /// daemon that came up with the listener disabled or a hand-edited
    /// lock file. The caller has no address to probe.
    Unprobeable,
    /// The lock names a control-plane socket that is not the one a
    /// daemon owning this lock binds — another directory, another
    /// name, a symbolic link, or not a socket. Carries the reason,
    /// which names the path. The endpoint is never dialed.
    InvalidEndpoint(String),
}

/// Probe the daemon recorded in `info`, the contents of the session
/// lock at `lock_path`, giving up after `timeout`.
///
/// `timeout` bounds both the connect step and the `daemon.version`
/// round-trip independently, so a hung listener still resolves inside
/// twice the configured budget.
pub async fn probe(lock_path: &Path, info: &LockInfo, timeout: Duration) -> HealthProbe {
    let Some(sock_path) = info.control_sock.clone() else {
        return HealthProbe::Unprobeable;
    };
    match check_lock_endpoint(lock_path, &sock_path) {
        LockEndpointState::Valid => {}
        LockEndpointState::Missing => return HealthProbe::Stale,
        LockEndpointState::Invalid(reason) => return HealthProbe::InvalidEndpoint(reason),
    }
    let socket = ControlSocket::from_path(sock_path.clone());
    let connect_fut = connect(&socket);
    let client = match tokio::time::timeout(timeout, connect_fut).await {
        Ok(Ok(c)) => c,
        _ => return HealthProbe::Stale,
    };
    let call_fut = client.call_raw("daemon.version", Value::Null);
    match tokio::time::timeout(timeout, call_fut).await {
        Ok(Ok(_)) => HealthProbe::Healthy(info.pid, sock_path),
        _ => HealthProbe::Stale,
    }
}

/// What a session lock says about a daemon.
#[derive(Debug)]
pub enum SessionLockState {
    /// No lock file, or lock content whose `flock` nobody holds — what
    /// a crashed daemon leaves behind.
    NotHeld { lock_path: PathBuf },
    /// The lock could not be read or parsed, or its `flock` could not
    /// be probed.
    Unreadable {
        lock_path: PathBuf,
        error: eyre::Report,
    },
    /// A process holds the lock; `probe` is what the daemon it records
    /// answered.
    Held {
        lock_path: PathBuf,
        info: LockInfo,
        probe: HealthProbe,
    },
}

/// Classify the session lock at `lock_path`: read it, see whether a
/// process holds it, and probe the daemon it records with
/// [`SESSION_PROBE_TIMEOUT`].
pub async fn classify_session_lock_at(lock_path: &Path) -> SessionLockState {
    let lock_path = lock_path.to_path_buf();
    match bookrack_session::inspect_session_lock_at(&lock_path) {
        Ok(None) => SessionLockState::NotHeld { lock_path },
        Err(error) => SessionLockState::Unreadable { lock_path, error },
        Ok(Some(info)) => {
            let probe = probe(&lock_path, &info, SESSION_PROBE_TIMEOUT).await;
            SessionLockState::Held {
                lock_path,
                info,
                probe,
            }
        }
    }
}

/// [`classify_session_lock_at`] on the session lock of the runtime
/// directory `runtime_dir` resolves to.
pub async fn classify_session_lock(runtime_dir: Option<&Path>) -> eyre::Result<SessionLockState> {
    let dir = bookrack_session::resolve_runtime_dir(runtime_dir)?;
    Ok(classify_session_lock_at(&dir.join(bookrack_session::tty_lock_name())).await)
}
