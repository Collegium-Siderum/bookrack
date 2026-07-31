// SPDX-License-Identifier: Apache-2.0

//! The `OnceLock` trap, pinned.
//!
//! A binary of its own, and one test in it: the assertion is about what
//! happens the *second* time this process is initialised, so nothing
//! else may have initialised it first.

use bookrack_test_support::{ProcessEnv, process_env};

/// Two different specs in one test binary is a panic naming both,
/// never a silently discarded second request. `get_or_init` runs its
/// closure once and returns the stored value to every later caller, so
/// without this check the second caller would receive a sandbox built
/// to someone else's spec and read it as its own.
#[test]
#[should_panic(expected = "already installed a different process environment")]
fn two_specs_in_one_binary_is_a_panic() {
    process_env(ProcessEnv::isolated());
    process_env(ProcessEnv::daemon());
}
