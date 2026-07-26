// SPDX-License-Identifier: Apache-2.0

//! Which library the `index-profile` verbs act on.
//!
//! The two library-selecting verbs — `current` and `apply` — resolve
//! their data root through the shared resolver, so the whole precedence
//! ladder applies: `--data-dir`, `--library`, the data-root variable,
//! the portable layout, then a registry `default`. These tests drive the
//! real binary and assert on the root it names, because the failure mode
//! being pinned is silent: a verb that ignored a selector answered a
//! question about a different library without saying so.
//!
//! Every case sets `BOOKRACK_REGISTRY` explicitly, so nothing here reads
//! the platform-default registry of the machine running the suite.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// One test's isolated world: a registry file, a runtime directory, and
/// two data roots with stable basenames (`alpha`, `beta`) so an assertion
/// can name the root it expects. `beta` exists only as a registry target,
/// reached through the `{beta}` placeholder in the registry body.
struct World {
    _root: tempfile::TempDir,
    registry: PathBuf,
    runtime: PathBuf,
    alpha: PathBuf,
}

impl World {
    fn new(registry_body: &str) -> World {
        let root = tempfile::tempdir().expect("tempdir");
        let alpha = root.path().join("alpha");
        let beta = root.path().join("beta");
        let runtime = root.path().join("runtime");
        for dir in [&alpha, &beta, &runtime] {
            std::fs::create_dir_all(dir).expect("create dir");
        }
        let registry = root.path().join("registry.toml");
        let body = registry_body
            .replace("{alpha}", alpha.to_str().expect("utf-8 path"))
            .replace("{beta}", beta.to_str().expect("utf-8 path"));
        std::fs::write(&registry, body).expect("write registry");
        World {
            _root: root,
            registry,
            runtime,
            alpha,
        }
    }

    /// Run the binary with this world's registry and runtime directory.
    /// `data_dir_env` sets the data-root variable; `None` removes it.
    fn run(&self, args: &[&str], data_dir_env: Option<&Path>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bookrack"));
        cmd.args(args)
            .env("BOOKRACK_REGISTRY", &self.registry)
            .env("BOOKRACK_RUNTIME_DIR", &self.runtime);
        match data_dir_env {
            Some(dir) => cmd.env("BOOKRACK_DATA_DIR", dir),
            None => cmd.env_remove("BOOKRACK_DATA_DIR"),
        };
        cmd.output().expect("spawn bookrack")
    }

    fn registry_text(&self) -> String {
        std::fs::read_to_string(&self.registry).expect("read registry")
    }

    fn manifest_text(&self, data_dir: &Path) -> Option<String> {
        std::fs::read_to_string(data_dir.join("bookrack-library.toml")).ok()
    }
}

fn stdout_of(output: &Output, what: &str) -> String {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{what} should exit 0; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A registry carrying `beta` as its default, so any verb that reads the
/// default alone lands on `beta` and an assertion naming `alpha` fails.
const BETA_IS_DEFAULT: &str = "default = \"beta\"\n\
     [libraries.beta]\n\
     data_dir = \"{beta}\"\n\
     kind = \"test\"\n";

/// `--data-dir` selects the library `current` reports on. It sits at the
/// top of the ladder, so it wins over a registry `default` naming another
/// root — rather than being accepted and dropped.
#[test]
fn the_data_dir_flag_wins_over_the_registry_default() {
    let world = World::new(BETA_IS_DEFAULT);
    let out = world.run(
        &[
            "index-profile",
            "current",
            "--json",
            "--data-dir",
            world.alpha.to_str().expect("utf-8 path"),
        ],
        None,
    );
    let stdout = stdout_of(&out, "current --data-dir");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("current --json is JSON");
    assert_eq!(
        value["data_dir"],
        serde_json::json!(world.alpha.to_str().expect("utf-8 path")),
        "the flag's root must be the one reported: {stdout}",
    );
    assert_eq!(
        value["registered"],
        serde_json::json!(false),
        "alpha is not a registry entry: {stdout}",
    );
    assert!(
        !stdout.contains("beta"),
        "the registry default must not leak into the report: {stdout}",
    );
}

/// The same for the data-root variable, on both verbs, with a registry
/// that has no default at all: the variable stands on its own, and a
/// machine with an empty registry is not a machine where these verbs
/// stop working.
#[test]
fn the_data_root_variable_alone_resolves_both_verbs() {
    let world = World::new("");

    let stdout = stdout_of(
        &world.run(&["index-profile", "current"], Some(&world.alpha)),
        "current under the data-root variable",
    );
    assert!(
        stdout.contains(&format!("library: alpha ({})", world.alpha.display())),
        "current must name the variable's root: {stdout}",
    );
    assert!(
        stdout.contains("not in the registry"),
        "an unregistered root must be reported as one: {stdout}",
    );

    let stdout = stdout_of(
        &world.run(
            &["index-profile", "apply", "qwen3-0.6b-default", "--dry-run"],
            Some(&world.alpha),
        ),
        "apply --dry-run under the data-root variable",
    );
    assert!(
        stdout.contains(&format!("library: alpha ({})", world.alpha.display())),
        "the plan must be derived against the variable's root: {stdout}",
    );
}

/// Executing `apply` on a root the registry does not carry writes the
/// declaration the manifest owns and leaves the registry alone: there is
/// no cached copy to refresh, and minting an entry is `libraries add`'s
/// decision, not this command's.
#[test]
fn apply_declares_into_an_unregistered_root_without_touching_the_registry() {
    let world = World::new("");
    let stdout = stdout_of(
        &world.run(
            &["index-profile", "apply", "qwen3-0.6b-default", "--yes"],
            Some(&world.alpha),
        ),
        "apply on an unregistered root",
    );
    assert!(
        stdout.contains("declared: index_profile = 'qwen3-0.6b-default' (library manifest)"),
        "the manifest declaration is the point of the command: {stdout}",
    );
    assert!(
        stdout.contains("the registry does not carry this data root"),
        "the skipped cache refresh must be said out loud: {stdout}",
    );

    let manifest = world
        .manifest_text(&world.alpha)
        .expect("apply mints a manifest for a root that had none");
    assert!(
        manifest.contains("index_profile = \"qwen3-0.6b-default\""),
        "the manifest must carry the declaration: {manifest}",
    );
    assert!(
        manifest.contains("name = \"alpha\""),
        "a minted manifest is named after the root it describes: {manifest}",
    );
    assert_eq!(
        world.registry_text(),
        "",
        "an unregistered root must not gain a registry entry",
    );
}

/// The registry leg still runs for a root the registry does carry: the
/// entry's cached `index_profile` is refreshed alongside the manifest,
/// and resolution through the registry `default` still reaches it.
#[test]
fn apply_refreshes_the_cached_copy_for_a_registered_root() {
    let world = World::new(
        "default = \"alpha\"\n\
         [libraries.alpha]\n\
         data_dir = \"{alpha}\"\n\
         kind = \"test\"\n",
    );
    let stdout = stdout_of(
        &world.run(
            &["index-profile", "apply", "qwen3-0.6b-default", "--yes"],
            None,
        ),
        "apply on the registry default",
    );
    assert!(
        stdout.contains(&format!("library: alpha ({})", world.alpha.display())),
        "the registry default must resolve to its own root: {stdout}",
    );
    assert!(
        !stdout.contains("the registry does not carry this data root"),
        "a registered root has a cache to refresh: {stdout}",
    );

    let manifest = world
        .manifest_text(&world.alpha)
        .expect("apply mints a manifest for a root that had none");
    assert!(
        manifest.contains("index_profile = \"qwen3-0.6b-default\""),
        "{manifest}",
    );
    let registry = world.registry_text();
    assert!(
        registry.contains("index_profile = \"qwen3-0.6b-default\""),
        "the registry cache must be refreshed: {registry}",
    );
}
