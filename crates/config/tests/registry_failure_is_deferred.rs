// SPDX-License-Identifier: Apache-2.0

//! A registry that cannot be read is fatal only where it is needed.
//!
//! `BOOKRACK_REGISTRY` points at a file this binary never creates, so
//! every resolution below starts from a registry read failure. What
//! differs is whether the resolution needed the registry at all.

use bookrack_config::{Config, LibrarySelection, ResolutionSource};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};

/// The registry variable names this, and nothing creates it.
const MISSING: &str = "never-written-registry.toml";

fn world() -> &'static Sandbox {
    process_env(
        ProcessEnv::isolated()
            .without_data_dir()
            .registry_absent(MISSING),
    )
}

/// A root fixed by `--data-dir` consults no registry, so an unreadable
/// one must not veto it. What it loses is the annotation layer, and the
/// loss is recorded rather than silent.
///
/// Before the deferral this was the `?` on the registry load: the whole
/// resolution failed with `RegistryUnreadable` even though nothing in
/// it was going to read a registry.
#[test]
fn a_pinned_root_survives_an_unreadable_registry() {
    let sandbox = world();
    let root = sandbox.data_root("explicit");
    let config = Config::resolve(&LibrarySelection {
        data_dir: Some(root.clone()),
        library: None,
    })
    .unwrap_or_else(|e| panic!("a root named by the flag must not be vetoed by the registry: {e}"));

    assert_eq!(config.source(), ResolutionSource::DataDirFlag);
    assert_eq!(config.data_dir(), root.as_path());

    let record = config
        .unusable_registry()
        .expect("the read failure must be recorded, not dropped");
    assert_eq!(record.path, sandbox.path().join(MISSING));
    assert!(
        record.reason.starts_with("cannot be read: "),
        "the record must say why: {}",
        record.reason,
    );
    assert_eq!(config.shadowed_default(), None);
    assert_eq!(config.library_identification(), None);
}

/// A guardrail, not a red-then-green test: this passes before the
/// deferral and after it. A naive "defer everything" implementation
/// makes it fail, which is the point — `--library` has no table to look
/// in, so the failure has to reach the caller.
#[test]
fn a_library_selection_still_fails_on_an_unreadable_registry() {
    world();
    let err = Config::resolve(&LibrarySelection {
        data_dir: None,
        library: Some("anything".to_string()),
    })
    .expect_err("a selection that needs the registry must fail");
    let message = err.to_string();
    assert!(
        message.contains(MISSING),
        "the failure must name the registry it could not read: {message}",
    );
}

/// The same guardrail one rung lower: with nothing selected at all, the
/// ladder falls off the end and would report "no library configured".
/// That is the symptom; the read failure is the cause, and the cause is
/// what reaches the caller — otherwise a typo in the registry path
/// looks like a machine that was never set up, and the CLI offers to
/// run the first-run wizard against it.
#[test]
fn an_unconfigured_resolution_reports_the_registry_not_the_missing_root() {
    world();
    let err =
        Config::resolve(&LibrarySelection::default()).expect_err("nothing configures a root here");
    let message = err.to_string();
    assert!(
        message.contains(MISSING),
        "the failure must name the registry, not the missing root: {message}",
    );
    assert!(
        !message.contains("no library configured"),
        "the symptom must not stand in for the cause: {message}",
    );
}
