// SPDX-License-Identifier: Apache-2.0

//! Daemon integration tests that install `ProcessEnv::daemon().without_data_dir()`.
//!
//! One binary installs one process environment, so every module here
//! installs this spec and no other; the guard below holds the line.

#[path = "../common/mod.rs"]
mod common;

bookrack_test_support::test_group!(
    "daemon_no_data_dir",
    spec = Some("ProcessEnv::daemon().without_data_dir()")
);

mod auto_registration;
mod dry_run_events;
mod eager_mount;
mod library_default;
mod library_key_declaration;
mod multi_library_config;
mod multi_library_params;
mod relocated_identity_refusal;
mod routed_diagnostics;
mod runtime_mount;
mod submit_library_routing;
mod unknown_library_is_symmetric;
mod write_reply_names_library;
