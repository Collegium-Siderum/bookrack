// SPDX-License-Identifier: Apache-2.0

//! `BOOKRACK_REGISTRY` names the only registry a resolution consults.
//!
//! These go through the impure [`bookrack_config::Config::resolve`]
//! rather than the pure layer underneath it, because the claim is about
//! what the process reads from the host — and that is exactly what a
//! pure test cannot see. The isolation comes from
//! `bookrack_test_support`, which redirects the platform config
//! directory into a sandbox so this file can write a "platform-default"
//! registry and know the binary would otherwise have found it.

use std::sync::OnceLock;

use bookrack_config::{Config, LibrarySelection, ResolutionSource};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};

/// The name the platform-default registry uses for its default library.
/// Any leak of that registry into a resolution shows up as this string.
const PLATFORM: &str = "platform";

/// A sandbox carrying two registries: the one the environment names,
/// holding a single entry and **no** `default`, and a platform-default
/// one whose `default` would win if it were ever consulted.
fn world() -> &'static Sandbox {
    static SEEDED: OnceLock<()> = OnceLock::new();
    let sandbox = process_env(ProcessEnv::isolated().without_data_dir());
    SEEDED.get_or_init(|| {
        let pinned = sandbox.data_root("pinned");
        sandbox.write_registry_entries(None, &[("pinned", pinned.as_path())]);

        // Written through the path the code under test computes, not a
        // hand-rolled per-platform join: if the redirection did not take,
        // this write lands outside the sandbox and the assertion below
        // catches it before any test asserts anything else.
        let platform_path =
            bookrack_config::default_registry_path().expect("a platform config directory");
        assert!(
            platform_path.starts_with(sandbox.path()),
            "the platform registry path {} is outside the sandbox at {}; \
             the home redirection did not take",
            platform_path.display(),
            sandbox.path().display(),
        );
        std::fs::create_dir_all(platform_path.parent().expect("a parent directory"))
            .expect("create the platform config directory");
        let platform_root = sandbox.data_root(PLATFORM);
        std::fs::write(
            &platform_path,
            format!(
                "default = \"{PLATFORM}\"\n\n[libraries.{PLATFORM}]\ndata_dir = {:?}\n",
                platform_root.display().to_string(),
            ),
        )
        .expect("write the platform registry");
    });
    sandbox
}

/// A pinned registry with no `default` ends the ladder. The last rung —
/// the platform-default registry's `default` — must not answer for it.
///
/// Before the suppression this was green for the wrong reason: rung 5
/// found no default, rung 6 read the machine's own registry, and the
/// resolution succeeded against a root the caller never named.
#[test]
fn a_pinned_registry_without_a_default_does_not_fall_through_to_the_platform_one() {
    let sandbox = world();
    match Config::resolve(&LibrarySelection::default()) {
        Ok(config) => panic!(
            "resolution reached {} via {:?}; the platform-default registry \
             answered a question the pinned registry had already ended",
            config.data_dir().display(),
            config.source(),
        ),
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("no library configured"),
                "expected the unconfigured verdict, got: {message}",
            );
            assert!(
                !message.contains(PLATFORM),
                "the platform registry must not appear in the failure: {message}",
            );
            assert!(sandbox.path().is_dir());
        }
    }
}

/// A root fixed by `--data-dir` is annotated from the pinned registry
/// or not at all. The platform-default registry must contribute neither
/// a shadowed default nor a library name.
///
/// Before the suppression, `detect_shadowed_default` reached past the
/// pinned registry to the platform one and reported its `default` as
/// eclipsed — the machine's own configuration showing up in the output
/// of a fully pinned run.
#[test]
fn a_path_class_root_is_not_shadowed_by_the_platform_registry() {
    let sandbox = world();
    let root = sandbox.data_root("explicit");
    let config = Config::resolve(&LibrarySelection {
        data_dir: Some(root.clone()),
        library: None,
    })
    .expect("a root named by the flag resolves");

    assert_eq!(config.source(), ResolutionSource::DataDirFlag);
    assert_eq!(config.data_dir(), root.as_path());
    assert_eq!(
        config.shadowed_default(),
        None,
        "the platform registry's default was reported as eclipsed: {:?}",
        config.shadowed_default(),
    );
    assert_eq!(config.library_identification(), None);
    assert_eq!(config.library(), None);
}
