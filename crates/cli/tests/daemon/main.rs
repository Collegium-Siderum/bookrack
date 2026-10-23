// SPDX-License-Identifier: Apache-2.0

//! Integration tests that install `ProcessEnv::daemon()` and run a daemon in process.
//!
//! One binary installs one process environment, so every module here
//! installs this spec and no other; the guard below holds the line.

bookrack_test_support::test_group!("daemon", spec = Some("ProcessEnv::daemon()"));

mod abandoned_runs;
mod doctor_mcp_endpoint;
mod mcp_parity;
mod second_launch;
