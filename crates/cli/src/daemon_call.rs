// SPDX-License-Identifier: Apache-2.0

//! Bounds every call the CLI makes to the daemon runs under.
//!
//! Held on the library side rather than beside the client helpers that
//! apply them, because the inventory `config fixed` prints is assembled
//! from the library: a value only the binary can name is a value an
//! operator cannot look up.

use std::time::Duration;

/// Per-RPC timeout applied to every control client the CLI builds.
/// Sized generously so steady-state operations never trip it on a
/// healthy daemon while still catching one that has wedged.
// setting: cli.call_timeout
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Stall timeout for a wait on queued jobs: how long the loop tolerates
/// zero events before reporting that the daemon has stopped
/// progressing. The timer resets on every event seen, so a long job
/// that keeps emitting progress survives regardless of total elapsed
/// time.
// setting: cli.await_stall_timeout
pub const DEFAULT_AWAIT_STALL_TIMEOUT: Duration = Duration::from_secs(60);
