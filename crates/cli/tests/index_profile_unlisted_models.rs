// SPDX-License-Identifier: Apache-2.0

//! Models outside the built-in registry are a remark, not a refusal.
//!
//! The registry records what is known about a few models; a profile
//! naming another one validates on its structure alone, and both write
//! entrances accept it. What the registry does know stays binding: a
//! listed model must be declared at the dimension it emits, and the
//! deprecated `--allow-unknown-model` no longer waives that.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

const UNLISTED_EMBED: &str = "synthetic-embed:unlisted";
const UNLISTED_RERANKER: &str = "synthetic-reranker:unlisted";

/// A profile file body. `reranker` is the `[reranker]` section's body.
fn profile_toml(name: &str, model: &str, dim: u32, reranker: &str) -> String {
    format!(
        "schema_version = 1\n\
         name = \"{name}\"\n\
         description = \"A synthetic profile.\"\n\
         \n\
         [embed]\n\
         backend = \"ollama\"\n\
         model = \"{model}\"\n\
         dim = {dim}\n\
         \n\
         [ann]\n\
         kind = \"brute-force\"\n\
         num_partitions = 1\n\
         num_bits = 8\n\
         nprobes = 1\n\
         refine_factor = 1\n\
         \n\
         [reranker]\n\
         {reranker}"
    )
}

const NO_RERANKER: &str = "kind = \"none\"\n";

fn unlisted_cross_encoder() -> String {
    format!(
        "kind = \"cross-encoder\"\n\
         backend = \"llama-server\"\n\
         model = \"{UNLISTED_RERANKER}\"\n\
         top_k_in = 50\n\
         top_k_out = 10\n"
    )
}

/// A registry with one library, `alpha`, and the user profile directory
/// beside it.
struct Fixture {
    sandbox: Sandbox,
    _registry_dir: tempfile::TempDir,
    registry_path: PathBuf,
    profiles: PathBuf,
    data_dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Fixture> {
        let sandbox = Sandbox::new();
        let registry_dir = tempfile::tempdir()?;
        let data_dir = tempfile::tempdir()?;
        let registry_path = registry_dir.path().join("registry.toml");
        std::fs::write(
            &registry_path,
            format!(
                "[libraries.alpha]\n\
                 data_dir = \"{}\"\n\
                 kind = \"test\"\n",
                data_dir.path().display()
            ),
        )?;
        let profiles = registry_dir.path().join("index-profiles");
        std::fs::create_dir_all(&profiles)?;
        Ok(Fixture {
            sandbox,
            _registry_dir: registry_dir,
            registry_path,
            profiles,
            data_dir,
        })
    }

    fn write_profile(&self, name: &str, body: &str) -> Result<()> {
        std::fs::write(self.profiles.join(format!("{name}.toml")), body)?;
        Ok(())
    }

    fn data_dir(&self) -> &Path {
        self.data_dir.path()
    }

    async fn run(&self, args: &[&str]) -> Result<(Option<i32>, String, String)> {
        let output = tokio::process::Command::from(
            bookrack_cmd!(&self.sandbox)
                .registry(&self.registry_path)
                .without_data_dir()
                .build(),
        )
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
        Ok((
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unlisted_model_validates_and_the_old_flag_is_a_deprecated_no_op() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_profile(
        "unlisted",
        &profile_toml("unlisted", UNLISTED_EMBED, 8, NO_RERANKER),
    )?;

    let (code, stdout, stderr) = fx.run(&["index-profile", "validate", "unlisted"]).await?;
    assert_eq!(
        code,
        Some(0),
        "an unlisted model is not a validation error: {stdout} {stderr}"
    );
    assert!(!stderr.contains("deprecated"), "{stderr}");

    let (code, stdout, stderr) = fx
        .run(&[
            "index-profile",
            "validate",
            "unlisted",
            "--allow-unknown-model",
        ])
        .await?;
    assert_eq!(code, Some(0), "{stdout} {stderr}");
    assert!(
        stderr.contains("--allow-unknown-model is deprecated"),
        "the flag says it does nothing now: {stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listed_model_at_a_foreign_dimension_fails_with_or_without_the_flag() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_profile(
        "foreign-dim",
        &profile_toml("foreign-dim", "qwen3-embedding:0.6b", 8, NO_RERANKER),
    )?;
    for extra in [None, Some("--allow-unknown-model")] {
        let mut args = vec!["index-profile", "validate", "foreign-dim"];
        args.extend(extra);
        let (code, stdout, stderr) = fx.run(&args).await?;
        assert_eq!(
            code,
            Some(1),
            "{extra:?}: the registry's dimension for a listed model binds: {stdout} {stderr}"
        );
        assert!(stdout.contains("embed.dim"), "{extra:?}: {stdout}");
        assert!(!stdout.contains("--allow"), "{extra:?}: {stdout}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_write_entrances_accept_unlisted_models() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_profile(
        "unlisted-stack",
        &profile_toml(
            "unlisted-stack",
            UNLISTED_EMBED,
            8,
            &unlisted_cross_encoder(),
        ),
    )?;

    let (code, stdout, stderr) = fx
        .run(&[
            "libraries",
            "config",
            "alpha",
            "index_profile=unlisted-stack",
        ])
        .await?;
    assert_eq!(
        code,
        Some(0),
        "libraries config must accept an unlisted model: {stdout} {stderr}"
    );
    let manifest = std::fs::read_to_string(fx.data_dir().join("bookrack-library.toml"))?;
    assert!(
        manifest.contains("index_profile = \"unlisted-stack\""),
        "{manifest}"
    );

    let (code, stdout, stderr) = fx
        .run(&[
            "--library",
            "alpha",
            "index-profile",
            "apply",
            "unlisted-stack",
            "--dry-run",
        ])
        .await?;
    assert_eq!(
        code,
        Some(0),
        "apply must accept an unlisted model: {stdout} {stderr}"
    );
    Ok(())
}
