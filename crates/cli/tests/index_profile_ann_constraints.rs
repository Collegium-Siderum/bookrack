// SPDX-License-Identifier: Apache-2.0

//! An index profile carrying an ANN setting no build accepts is refused
//! at both write entrances.
//!
//! A zero `refine_factor` is the case: a build used to accept it and
//! record it, and only search failed on it. `index-profile validate`
//! names the field; `libraries config` and `index-profile apply` refuse
//! the reference before writing anything and point at `validate`.

#![cfg(unix)]

use bookrack_test_support::{Sandbox, Spawn, bookrack_cmd};
use tokio::process::Command;

const LIBRARY: &str = "main";
const PROFILE: &str = "zero-refine";

/// The per-user profile directory, beside the registry file.
const USER_PROFILE_DIR: &str = "index-profiles";

fn profile_toml() -> String {
    format!(
        "schema_version = 1\n\
         name = \"{PROFILE}\"\n\
         description = \"An IVF index with a zero refine factor.\"\n\
         \n\
         [embed]\n\
         backend = \"ollama\"\n\
         model = \"synthetic-embed:constraints\"\n\
         dim = 8\n\
         \n\
         [ann]\n\
         kind = \"ivf-flat\"\n\
         num_partitions = 1\n\
         nprobes = 1\n\
         refine_factor = 0\n\
         \n\
         [reranker]\n\
         kind = \"none\"\n"
    )
}

/// Run one client command built from `spawn` and return its exit code,
/// stdout and stderr.
async fn run(spawn: Spawn, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::from(spawn.build())
        .args(args)
        .output()
        .await
        .expect("run the bookrack client");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn a_zero_refine_factor_is_refused_at_both_write_entrances() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root(LIBRARY);
    let registry = sandbox.write_registry_entries(Some(LIBRARY), &[(LIBRARY, root.as_path())]);
    let profiles = registry
        .parent()
        .expect("the registry has a parent")
        .join(USER_PROFILE_DIR);
    std::fs::create_dir_all(&profiles).expect("create the user profile directory");
    std::fs::write(profiles.join(format!("{PROFILE}.toml")), profile_toml())
        .expect("write the user profile");

    let (code, stdout, stderr) = run(
        bookrack_cmd!(&sandbox).without_data_dir(),
        &["index-profile", "validate", PROFILE],
    )
    .await;
    assert_eq!(code, Some(1), "validation fails: {stdout} {stderr}");
    assert!(
        stdout.contains("ann.refine_factor"),
        "validate names the field: {stdout}"
    );

    let set = format!("index_profile={PROFILE}");
    let entrances: [(&str, Vec<&str>); 2] = [
        (
            "libraries config",
            vec!["libraries", "config", LIBRARY, &set],
        ),
        (
            "index-profile apply",
            vec![
                "--library",
                LIBRARY,
                "index-profile",
                "apply",
                PROFILE,
                "--dry-run",
            ],
        ),
    ];
    for (entrance, args) in entrances {
        let (code, stdout, stderr) = run(bookrack_cmd!(&sandbox).without_data_dir(), &args).await;
        assert_eq!(code, Some(2), "{entrance} refuses: {stdout} {stderr}");
        assert!(
            stderr.contains(&format!("index profile '{PROFILE}' has validation errors")),
            "{entrance}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("bookrack index-profile validate {PROFILE}")),
            "{entrance} points at validate: {stderr}"
        );
    }
    let manifest = std::fs::read_to_string(root.join("bookrack-library.toml")).unwrap_or_default();
    assert!(
        !manifest.contains(PROFILE),
        "a refused reference is not written: {manifest}"
    );
}
