// SPDX-License-Identifier: Apache-2.0

//! Command-line integration tests that drive the `bookrack` binary as a child.
//!
//! One binary installs one process environment; this one installs none,
//! and the guard below refuses a module that would.

#[path = "../common/mod.rs"]
mod common;

bookrack_test_support::test_group!("cmd", spec = None);

mod config_effective;
mod config_knobs;
mod distill_run_lifecycle;
mod dotenv_entry_point;
mod error_rendering;
mod fixed_inventory_gate;
mod index_profile_ann_constraints;
mod index_profile_apply_selection;
mod index_profile_apply_self_check;
mod index_profile_resolution;
mod index_profile_unlisted_models;
mod init_refusal;
mod isolation;
mod knob_inventory_gate;
mod libraries_default_reaches_the_daemon;
mod libraries_list_served;
mod library_routing_e2e;
mod listing_names_its_library;
mod oneshot_e2e;
mod preflight_refusal;
mod proxy_bypass;
mod queue_document_refusal;
mod queue_list_filter;
mod queue_retry;
mod queue_short_id;
mod queue_wait;
mod quit_e2e;
mod registry_absolute_roots;
mod retrieval_weak_advisory;
mod rpc_e2e;
mod status_card_identity;
mod status_registered_at_startup;
mod verify_exit_code;
