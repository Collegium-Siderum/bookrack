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
//! - [`HealthProbe::Stale`] — the lock named a control-plane socket
//!   but no live daemon answered; the caller should ask the operator
//!   to remove the lock by hand.
//! - [`HealthProbe::Unprobeable`] — the lock did not name a
//!   control-plane socket; the caller falls back to the underlying
//!   acquire error.

use std::path::PathBuf;
use std::time::Duration;

use bookrack_control_client::{ControlSocket, connect};
use bookrack_session::LockInfo;
use serde_json::Value;

/// Verdict of a single probe attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthProbe {
    /// Daemon answered `daemon.version` inside the timeout. Carries
    /// the recorded pid and the control-plane socket path so the
    /// caller can surface either to the operator.
    Healthy(u32, PathBuf),
    /// The lock named a control-plane socket but no live daemon
    /// answered inside the timeout (connect failed or `daemon.version`
    /// did not return).
    Stale,
    /// The lock did not name a control-plane socket — typically a
    /// daemon that came up with the listener disabled or a hand-edited
    /// lock file. The caller has no address to probe.
    Unprobeable,
}

/// Probe the daemon recorded in `info`, giving up after `timeout`.
///
/// `timeout` bounds both the connect step and the `daemon.version`
/// round-trip independently, so a hung listener still resolves inside
/// twice the configured budget.
pub async fn probe(info: &LockInfo, timeout: Duration) -> HealthProbe {
    let Some(sock_path) = info.control_sock.clone() else {
        return HealthProbe::Unprobeable;
    };
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
