// SPDX-License-Identifier: Apache-2.0

//! Daemon integration tests that install `ProcessEnv::daemon()`.
//!
//! One binary installs one process environment, so every module here
//! installs this spec and no other; the guard below holds the line.

#[path = "../common/mod.rs"]
mod common;

bookrack_test_support::test_group!("daemon", spec = Some("ProcessEnv::daemon()"));

mod control_error_codes;
mod control_minimal;
mod control_writes;
mod crash_replay;
mod daemon_lifecycle;
mod doctor_gather;
mod glean_submit;
mod mcp_endpoint;
mod metadata_write_reports;
mod papers_maintenance;
mod papers_metadata_writes;
mod queue_document_gate;
mod queue_failure_problem;
mod queue_lifecycle;
mod queue_retry;
mod source_path_redaction;
mod source_path_sweep;
mod unusable_vector_ingest;
mod vector_reset_failure;
mod vector_write_reports;
mod worker_progress;
mod workerless_dispatch;
