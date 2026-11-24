// SPDX-License-Identifier: Apache-2.0

//! Daemon integration tests that install `ProcessEnv::daemon()`.
//!
//! One binary installs one process environment, so every module here
//! installs this spec and no other; the guard below holds the line.

#[path = "../common/mod.rs"]
mod common;

bookrack_test_support::test_group!("daemon", spec = Some("ProcessEnv::daemon()"));

mod auto_registration;
mod control_error_codes;
mod control_minimal;
mod control_writes;
mod crash_replay;
mod daemon_config;
mod daemon_identity;
mod daemon_lifecycle;
mod doctor_gather;
mod dry_run_events;
mod eager_mount;
mod glean_submit;
mod identity_manifest_refusal;
mod library_default;
mod library_key_declaration;
mod library_methods;
mod mcp_endpoint;
mod metadata_write_reports;
mod multi_library_config;
mod multi_library_params;
mod papers_maintenance;
mod papers_metadata_writes;
mod queue_announcements;
mod queue_document_gate;
mod queue_failure_problem;
mod queue_lifecycle;
mod queue_retry;
mod relocated_identity_refusal;
mod retired_hold;
mod routed_diagnostics;
mod rule_job;
mod rule_submit;
mod runtime_mount;
mod search_ann_overrides;
mod search_backend_failure;
mod skip_and_degrade;
mod submit_library_routing;
mod unknown_library_is_symmetric;
mod unnamed_routing;
mod unusable_vector_ingest;
mod vector_reset_failure;
mod vector_write_reports;
mod worker_progress;
mod workerless_dispatch;
mod write_alongside_job;
mod write_reply_names_library;
