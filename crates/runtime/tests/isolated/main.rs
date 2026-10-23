// SPDX-License-Identifier: Apache-2.0

//! Integration tests that install `ProcessEnv::isolated()`.
//!
//! One binary installs one process environment, so every module here
//! installs this spec and no other; the guard below holds the line.

bookrack_test_support::test_group!("isolated", spec = Some("ProcessEnv::isolated()"));

mod index_profile_plan;
mod reranker_model_refusal;
mod wizard;
