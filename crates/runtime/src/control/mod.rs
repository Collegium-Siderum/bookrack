// SPDX-License-Identifier: Apache-2.0

//! Daemon-side control-plane skeleton.
//!
//! Hosts the local IPC listener — a Unix domain socket on Unix-likes,
//! a named pipe on Windows — that the desktop tray, REPL extraction,
//! and future MCP/`bookrack exec` clients reach the daemon through.
//!
//! Wire format is newline-delimited JSON-RPC 2.0 (one frame per line);
//! see [`jsonrpc`] for the encoder/decoder, [`methods`] for the method
//! table, [`events`] for the broadcast channel that fans out
//! `daemon.state` / `queue.tick` / `worker.progress` /
//! `library.changed` / `mcp.availability`, and [`progress`] for the
//! sink the queue runner reports stage transitions through.

pub mod events;
pub mod jsonrpc;
pub mod methods;
pub mod probe;
pub mod progress;
pub mod socket;

pub use events::{DaemonState, DaemonStateFlag, Event, EventStreamHandle};
pub use probe::{HealthProbe, probe};
pub use progress::{EventProgressSink, NoopProgressSink, ProgressSink};
pub use socket::{BoundListener, ControlSocketPath, bind, run_accept_loop};
