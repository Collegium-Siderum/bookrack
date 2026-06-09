// SPDX-License-Identifier: Apache-2.0

//! Daemon-side control-plane skeleton.
//!
//! Hosts the local IPC listener — a Unix domain socket on Unix-likes,
//! a named pipe on Windows — that the desktop tray, REPL extraction,
//! and future MCP/`bookrack exec` clients reach the daemon through.
//!
//! Wire format is newline-delimited JSON-RPC 2.0 (one frame per line);
//! see [`jsonrpc`] for the encoder/decoder, [`methods`] for the
//! Phase 1 method table, and [`events`] for the broadcast channel
//! that fans out `daemon.state` and (in later phases) queue/library/
//! mcp notifications.

pub mod events;
pub mod jsonrpc;
pub mod methods;
pub mod socket;

pub use events::{DaemonState, DaemonStateFlag, Event, EventStreamHandle};
pub use socket::{BoundListener, ControlSocketPath, bind, run_accept_loop};
