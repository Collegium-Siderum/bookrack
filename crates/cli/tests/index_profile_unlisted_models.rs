// SPDX-License-Identifier: Apache-2.0

//! Models outside the built-in registry are a remark, not a refusal.
//!
//! The registry records what is known about a few models; a profile
//! naming another one validates on its structure alone, and both write
//! entrances accept it. What the registry does know stays binding: a
//! listed model must be declared at the dimension it emits, and the
//! deprecated `--allow-unknown-model` no longer waives that.
//!
//! A write entrance says what the registry does not know once per
//! library: a remark already shown for a library is not shown again
//! until what it says changes, and a remark a `--json` or `--quiet`
//! write did not print counts as not shown.

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

/// A registry with two libraries, `alpha` and `beta`, and the user
/// profile directory beside it.
struct Fixture {
    sandbox: Sandbox,
    _registry_dir: tempfile::TempDir,
    registry_path: PathBuf,
    profiles: PathBuf,
    data_dir: tempfile::TempDir,
    _beta_dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Fixture> {
        let sandbox = Sandbox::new();
        let registry_dir = tempfile::tempdir()?;
        let data_dir = tempfile::tempdir()?;
        let beta_dir = tempfile::tempdir()?;
        let registry_path = registry_dir.path().join("registry.toml");
        std::fs::write(
            &registry_path,
            format!(
                "[libraries.alpha]\n\
                 data_dir = \"{}\"\n\
                 kind = \"test\"\n\
                 \n\
                 [libraries.beta]\n\
                 data_dir = \"{}\"\n\
                 kind = \"test\"\n",
                data_dir.path().display(),
                beta_dir.path().display()
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
            _beta_dir: beta_dir,
        })
    }

    fn write_profile(&self, name: &str, body: &str) -> Result<()> {
        std::fs::write(self.profiles.join(format!("{name}.toml")), body)?;
        Ok(())
    }

    /// `libraries config <library> index_profile=<profile>`.
    async fn set_profile(
        &self,
        library: &str,
        profile: &str,
    ) -> Result<(Option<i32>, String, String)> {
        let arg = format!("index_profile={profile}");
        self.run(&["libraries", "config", library, &arg]).await
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

/// The note naming an unlisted embed model.
fn embed_note(model: &str) -> String {
    format!("note: embed model '{model}' is not in the model registry")
}

/// The note naming an unlisted cross-encoder model.
fn reranker_note(model: &str) -> String {
    format!("note: reranker model '{model}' is not in the model registry")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn libraries_config_notes_each_remark_once_per_library() -> Result<()> {
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

    let (code, stdout, stderr) = fx.set_profile("alpha", "unlisted-stack").await?;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stdout.contains(&embed_note(UNLISTED_EMBED)),
        "first write: {stdout}"
    );
    assert!(
        stdout.contains(&reranker_note(UNLISTED_RERANKER)),
        "first write: {stdout}"
    );

    let (_, stdout, _) = fx.set_profile("alpha", "unlisted-stack").await?;
    assert!(
        !stdout.contains("not in the model registry"),
        "a remark already shown for this library: {stdout}"
    );

    // The same file, its reranker swapped: only the remark that changed.
    let other = "synthetic-reranker:other";
    fx.write_profile(
        "unlisted-stack",
        &profile_toml(
            "unlisted-stack",
            UNLISTED_EMBED,
            8,
            &unlisted_cross_encoder().replace(UNLISTED_RERANKER, other),
        ),
    )?;
    let (_, stdout, _) = fx.set_profile("alpha", "unlisted-stack").await?;
    assert!(stdout.contains(&reranker_note(other)), "{stdout}");
    assert!(!stdout.contains(&embed_note(UNLISTED_EMBED)), "{stdout}");

    // Another library has been told nothing.
    let (_, stdout, _) = fx.set_profile("beta", "unlisted-stack").await?;
    assert!(stdout.contains(&embed_note(UNLISTED_EMBED)), "{stdout}");
    assert!(stdout.contains(&reranker_note(other)), "{stdout}");

    // A machine-readable or quiet write prints no note and records none.
    for (mode, model) in [
        ("--json", "synthetic-embed:json"),
        ("--quiet", "synthetic-embed:quiet"),
    ] {
        let name = format!("unlisted{mode}");
        fx.write_profile(&name, &profile_toml(&name, model, 8, NO_RERANKER))?;
        let arg = format!("index_profile={name}");
        let (code, stdout, stderr) = fx
            .run(&[mode, "libraries", "config", "alpha", &arg])
            .await?;
        assert_eq!(code, Some(0), "{mode}: {stderr}");
        assert!(!stdout.contains("note:"), "{mode}: {stdout}");
        if mode == "--json" {
            serde_json::from_str::<serde_json::Value>(stdout.trim())
                .unwrap_or_else(|e| panic!("--json stdout must stay JSON ({e}): {stdout}"));
        } else {
            assert!(stdout.trim().is_empty(), "{mode}: {stdout}");
        }
        let (_, stdout, _) = fx.run(&["libraries", "config", "alpha", &arg]).await?;
        assert!(
            stdout.contains(&embed_note(model)),
            "{mode} recorded a note it did not print: {stdout}"
        );
    }
    Ok(())
}
