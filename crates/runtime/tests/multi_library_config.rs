// SPDX-License-Identifier: Apache-2.0

//! Per-library configuration under an eager multi-mount daemon: every
//! mounted library must be served under its own declaration, not under
//! the bring-up-selected library's.
//!
//! The two libraries here differ only in the profile section under
//! test — the embed section is identical, so
//! `bookrack_test_support::EmbedStub` answers both probes and the
//! assertion isolates the section that matters.

#![cfg(unix)]

mod common;

use std::sync::OnceLock;

use bookrack_config::{LibraryKind, ManifestIdentitySeed, set_manifest_index_profile};
use bookrack_runtime::backend_probe::PreflightRefusal;
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, Sandbox, process_env};
use eyre::Result;

/// A user profile whose embed section matches the built-in default —
/// and so the model `EmbedStub` reports — carrying `reranker_section`
/// verbatim.
fn profile_toml(name: &str, reranker_section: &str) -> String {
    format!(
        r#"schema_version = 1
name = "{name}"
description = "Test profile."

[embed]
backend = "ollama"
model = "qwen3-embedding:0.6b"
dim = 1024

[ann]
kind = "ivf-pq"
num_partitions = 256
num_sub_vectors = 128
num_bits = 8
nprobes = 16
refine_factor = 10

{reranker_section}
"#
    )
}

const NO_RERANKER: &str = r#"[reranker]
kind = "none""#;

const CROSS_ENCODER: &str = r#"[reranker]
kind = "cross-encoder"
backend = "llama-server"
model = "Qwen3-Reranker-0.6B"
top_k_in = 50
top_k_out = 10"#;

/// Isolate the process and seed a two-library registry whose members
/// declare profiles that disagree on the reranker stage: `alpha` (the
/// registry default, and so the bring-up-selected library) declares
/// none, `beta` declares a cross-encoder.
///
/// `alpha` is the one without a reranker on purpose: with the profiles
/// read from the primary alone, bring-up asks for no backend at all, so
/// no supervised `llama-server` is ever spawned by this test.
fn world() -> &'static Sandbox {
    static SEEDED: OnceLock<()> = OnceLock::new();
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    SEEDED.get_or_init(|| {
        let profiles = sandbox.path().join("index-profiles");
        std::fs::create_dir_all(&profiles).expect("profile directory");
        std::fs::write(
            profiles.join("plain.toml"),
            profile_toml("plain", NO_RERANKER),
        )
        .expect("write plain profile");
        std::fs::write(
            profiles.join("reranked.toml"),
            profile_toml("reranked", CROSS_ENCODER),
        )
        .expect("write reranked profile");

        let alpha = sandbox.data_root("alpha-root");
        let beta = sandbox.data_root("beta-root");
        sandbox.write_registry_entries(
            Some("alpha"),
            &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
        );
        for (root, name, profile) in [
            (alpha.as_path(), "alpha", "plain"),
            (beta.as_path(), "beta", "reranked"),
        ] {
            set_manifest_index_profile(
                root,
                Some(profile),
                ManifestIdentitySeed {
                    name,
                    kind: LibraryKind::Test,
                    description: None,
                },
            )
            .expect("declare the library's index profile");
        }
    });
    sandbox
}

/// A set of one agrees with itself: the check must not turn the
/// non-eager path — a root the registry does not know, where the
/// mounted set holds exactly the primary — into a refusal.
///
/// The root taken here is unregistered on purpose. A path that *is*
/// registered resolves back to its registry name and mounts eagerly, so
/// selecting `alpha` by path would exercise the two-library case again
/// rather than the single-mount one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_mount_is_served_under_its_own_declaration() -> Result<()> {
    let sandbox = world();
    let runtime_root = tempfile::tempdir()?;

    let mut opts = RuntimeOpts::headless(Some(sandbox.data_root("solo-root")), None);
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());

    let runtime = DaemonRuntime::start(opts).await?;
    assert_eq!(
        runtime.registry.len(),
        1,
        "a path-selected root mounts once"
    );

    let shutdown_tx = runtime.shutdown_tx.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
    let _ = shutdown_tx.send(());
    runtime.run_until_shutdown(None, repl_handle).await?;
    Ok(())
}

/// Bring-up serves one reranker stage for the whole mounted set, so a
/// set whose profiles disagree on that stage cannot be served under any
/// of them: the refusal has to name the libraries that disagree.
///
/// Without this check the disagreement is silent — the primary's
/// profile decides, and a library declaring a cross-encoder is served
/// with no reranker at all, which is exactly the promise
/// `bring_up_reranker` documents that it upholds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mounted_set_whose_profiles_disagree_on_the_reranker_refuses_bring_up() -> Result<()> {
    world();
    let runtime_root = tempfile::tempdir()?;

    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());

    let err = match DaemonRuntime::start(opts).await {
        Ok(runtime) => {
            let shutdown_tx = runtime.shutdown_tx.clone();
            let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });
            let _ = shutdown_tx.send(());
            runtime.run_until_shutdown(None, repl_handle).await?;
            panic!(
                "bring-up served a mounted set whose profiles disagree on the reranker stage; \
                 expected a refusal naming both libraries"
            );
        }
        Err(e) => e,
    };

    // Assert on the `Problem` the CLI's exit-2 path carries, not on the
    // `Display` line: the detail sentence is what reaches the operator,
    // and naming both libraries is the point — a refusal that only says
    // "the profiles disagree" leaves them to find which two.
    let refusal = err
        .downcast_ref::<PreflightRefusal>()
        .unwrap_or_else(|| panic!("bring-up must refuse with a PreflightRefusal: {err:#}"));
    let detail = refusal
        .problem
        .data
        .detail
        .as_deref()
        .unwrap_or_else(|| panic!("the refusal carries no detail: {:?}", refusal.problem));
    for name in ["alpha", "beta"] {
        assert!(
            detail.contains(name),
            "the refusal must name {name}: {detail}"
        );
    }
    assert!(
        refusal
            .problem
            .data
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("index-profile current")),
        "the hint must point at the command that answers it: {:?}",
        refusal.problem
    );
    Ok(())
}
