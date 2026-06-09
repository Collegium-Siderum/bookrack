// SPDX-License-Identifier: Apache-2.0

//! Daemon-side runtime primitives shared by `bookrack run` and the
//! headless `bookrack-mcp` binary.
//!
//! Holds the business commands the CLI invokes (`cmd::*`), the
//! persistent ingest queue and worker (`queue`), the operator health
//! report (`doctor`), and the [`daemon::DaemonRuntime`] handle that
//! wires together the session lock, the library registry, the broadcast
//! channels, and the queue worker into one shutdown-coordinated
//! lifecycle. The CLI layer keeps argument parsing, TTY rendering, and
//! REPL dispatch; everything from the daemon edge down lives here.

pub mod audit_helpers;
pub mod cmd;
pub mod control;
pub mod daemon;
pub mod doctor;
pub mod embed_helpers;
pub mod ops_helpers;
pub mod queue;
pub mod render;

pub use daemon::{DaemonRuntime, LaunchMode, RuntimeOpts};
