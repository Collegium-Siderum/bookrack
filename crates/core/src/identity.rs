// SPDX-License-Identifier: Apache-2.0

//! The identity of one running daemon.

use serde::{Deserialize, Serialize};

/// What tells one daemon process from another, fixed when it starts.
///
/// Every surface that says which daemon is answering — the control
/// plane's `daemon.version` and `status`, the MCP `session.info` tool —
/// reports these six fields under these names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentity {
    /// A UUIDv7 drawn at start: distinct for every start, including a
    /// restart that reuses the pid and the runtime directory.
    pub instance_id: String,
    /// The workspace version the daemon was built from.
    pub version: String,
    /// The daemon's process id.
    pub pid: u32,
    /// The runtime directory holding the session lock, absolute.
    pub runtime_dir: String,
    /// The control-plane endpoint the daemon answers on; `null` for a
    /// dispatcher with no bound endpoint.
    pub control_sock: Option<String>,
    /// When the daemon entered service, RFC 3339.
    pub started_at: String,
}
