// SPDX-License-Identifier: Apache-2.0

//! The in-process half of the isolation, asserted through the real
//! impure entry points.
//!
//! A separate binary because it mutates this process's environment.

use bookrack_test_support::{ProcessEnv, process_env};

/// The platform paths a daemon test binary reads move into the
/// sandbox.
///
/// The assertion goes through `bookrack_config::default_registry_path`
/// and `bookrack_config::daemon_state_dir` rather than rebuilding the
/// join here, so the per-platform rule stays in one place: macOS
/// resolves the config directory from `HOME`, the Linux runner from
/// `XDG_CONFIG_HOME`, and this test covers whichever one it runs on
/// without this crate copying the distinction. Deleting
/// `XDG_CONFIG_HOME` from the installed set is green on macOS and red
/// on the runner.
#[test]
fn the_process_sandbox_moves_the_platform_paths() {
    let sandbox = process_env(ProcessEnv::daemon());

    let registry = bookrack_config::default_registry_path()
        .expect("the platform config directory resolves under the sandbox home");
    assert!(
        registry.starts_with(sandbox.path()),
        "the platform-default registry is at {}, outside the sandbox at {}",
        registry.display(),
        sandbox.path().display(),
    );

    let state = bookrack_config::daemon_state_dir().expect("the daemon state directory resolves");
    assert!(
        state.starts_with(sandbox.path()),
        "the daemon state directory is at {}, outside the sandbox at {}",
        state.display(),
        sandbox.path().display(),
    );
}

/// The `.env` channel is closed, not merely unlikely.
///
/// Cargo runs this binary from its package root, so dotenv's upward
/// search reaches the repository's own file; and because dotenv does
/// not overwrite variables that are already set, it would re-set
/// exactly the ones `process_env` removed. Suppressing the load is what
/// makes the isolation independent of that file's contents.
#[test]
fn the_process_sandbox_shuts_off_dotenv() {
    process_env(ProcessEnv::daemon());
    assert_eq!(
        std::env::var(bookrack_config::NO_DOTENV_ENV)
            .ok()
            .as_deref(),
        Some("1"),
        "a `.env` above the package root can still refill this environment",
    );
}

/// Asking for the same spec again is not an error: every test in a
/// binary opens with the same call, and they all see one tree.
#[test]
fn the_same_spec_returns_the_same_sandbox() {
    let first = process_env(ProcessEnv::daemon());
    let second = process_env(ProcessEnv::daemon());
    assert_eq!(first.path(), second.path());
}
