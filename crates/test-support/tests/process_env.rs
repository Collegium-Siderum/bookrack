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

/// The platform-default registry lands at the platform's conventional
/// location under the sandbox, not merely somewhere inside it.
///
/// `the_process_sandbox_moves_the_platform_paths` proves the
/// redirection took; this pins where the redirected path points. The
/// expected path is the platform convention `dirs` implements — the
/// `Application Support` directory under `HOME` on macOS,
/// `XDG_CONFIG_HOME` elsewhere — so a dependency move that changes
/// that convention turns this red while the prefix check above stays
/// green.
#[test]
fn the_platform_default_registry_sits_at_the_platform_conventional_path() {
    let sandbox = process_env(ProcessEnv::daemon());

    let registry = bookrack_config::default_registry_path()
        .expect("the platform config directory resolves under the sandbox home");

    #[cfg(target_os = "macos")]
    let config_dir = sandbox.home().join("Library").join("Application Support");
    #[cfg(not(target_os = "macos"))]
    let config_dir = sandbox.config_home();

    assert_eq!(
        registry,
        config_dir
            .join("bookrack")
            .join(bookrack_config::DEFAULT_REGISTRY_NAME),
    );
}

/// A daemon this binary brings up in process binds its MCP address
/// during bring-up, so the sandbox pins a kernel-assigned port: the
/// built-in default would hand one fixed host port a veto over
/// whether the test starts at all.
#[test]
fn the_process_sandbox_asks_for_a_kernel_assigned_mcp_port() {
    process_env(ProcessEnv::daemon());
    let daemon = bookrack_config::DaemonConfig::resolve(
        &bookrack_config::EnvSnapshot::capture(),
        &bookrack_config::DaemonFlags::default(),
    );
    assert_eq!(
        daemon.mcp.expect("an address resolves").addr,
        "127.0.0.1:0",
        "an in-process daemon must not contend for the default port",
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

/// The host's managed PDFium directory is read before the home
/// directory moves, and the product's own search moves with it.
///
/// Both halves go through the real resolvers. A lookup deferred until
/// the first sandbox asks for the host's library would run after the
/// redirection and name a directory inside the sandbox, so the opt-in
/// would link the sandbox to itself; and a `managed_data_dir` that
/// disagreed with the platform rule would link the library to a place
/// the product never searches.
#[test]
fn the_host_directory_is_fixed_before_the_environment_moves() {
    let sandbox = process_env(ProcessEnv::daemon());

    let host = bookrack_test_support::host_pdfium_root()
        .expect("the host's platform data directory resolves");
    assert!(
        !host.starts_with(sandbox.path()),
        "the host directory was read after the redirection: {}",
        host.display(),
    );

    let product = bookrack_config::pdfium_managed_root()
        .expect("the platform data directory resolves under the sandbox home");
    assert_eq!(
        product,
        sandbox.managed_data_dir().join("bookrack").join("pdfium"),
        "the sandbox links the library somewhere the product does not search",
    );
}
