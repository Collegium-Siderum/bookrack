// SPDX-License-Identifier: Apache-2.0

//! Effective index-profile resolution. A library can reference a profile
//! from two places — the per-root `config.toml` and its registry entry —
//! and the profile's embed model competes with the explicit `embed_model`
//! field and the env override. This module derives the single effective
//! combination (or refuses with the conflict spelled out), and every
//! embed-model consumer in this crate goes through it so the
//! `env > config.toml > profile > default` chain applies uniformly.

use std::path::{Path, PathBuf};

use bookrack_config::{Config, ConfigError, EmbedConfig, list_libraries, registry_target_path};
use bookrack_corpus::{Corpus, EMBED_MODEL_KEY, VECTOR_DIM_KEY};
use bookrack_index_profile::{
    IndexProfile, USER_PROFILE_DIR_NAME, ensure_reranker_supported, has_errors, resolve, validate,
};
use eyre::{Result, eyre};

/// Where a library's effective profile reference was declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileRefOrigin {
    /// Only `<data_root>/config.toml` names the profile.
    ConfigToml,
    /// Only the library's registry entry names the profile.
    Registry,
    /// Both name it, consistently.
    Both,
}

impl ProfileRefOrigin {
    /// Stable label for human and JSON rendering.
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileRefOrigin::ConfigToml => "config.toml",
            ProfileRefOrigin::Registry => "registry",
            ProfileRefOrigin::Both => "config.toml + registry",
        }
    }
}

/// The profile a library effectively runs under, with the reference that
/// selected it.
#[derive(Debug, Clone)]
pub struct EffectiveProfile {
    /// Which side(s) declared the reference.
    pub origin: ProfileRefOrigin,
    /// The resolved profile.
    pub profile: IndexProfile,
}

/// The per-user index-profile directory, beside `registry.toml`. `None`
/// when no config location resolves; built-in profiles still resolve by
/// name against the relative fallback the callers substitute.
pub fn user_profile_dir() -> Option<PathBuf> {
    registry_target_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .map(|d| d.join(USER_PROFILE_DIR_NAME))
}

/// [`user_profile_dir`] with the relative fallback every caller uses
/// when no config location resolves: user files then never match, so
/// only built-ins resolve.
pub fn user_profile_dir_or_default() -> PathBuf {
    user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME))
}

/// The `index_profile` the registry records for the library `cfg`
/// serves: matched by registry name when the selection carried one,
/// otherwise by data root. `None` when no entry matches or none records
/// a profile.
fn registry_profile_ref(cfg: &Config) -> Option<String> {
    let entries = list_libraries().ok().flatten()?;
    let entry = match cfg.library() {
        Some(name) => entries.iter().find(|e| e.name == name),
        None => entries
            .iter()
            .find(|e| same_dir(&e.data_dir, cfg.data_dir())),
    }?;
    entry.index_profile.clone()
}

/// Whether two paths name the same directory, comparing canonicalized
/// forms and falling back to a raw comparison when canonicalization
/// fails.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Pick the effective profile reference from the two independent
/// declaration sites, refusing when they disagree. Pure, so a test
/// drives every branch without a registry on disk.
pub(crate) fn effective_reference(
    config_ref: Option<String>,
    registry_ref: Option<String>,
) -> Result<Option<(String, ProfileRefOrigin)>, ConfigError> {
    match (config_ref, registry_ref) {
        (Some(c), Some(r)) if c != r => Err(ConfigError::profile_reference_conflict(&c, &r)),
        (Some(c), Some(_)) => Ok(Some((c, ProfileRefOrigin::Both))),
        (Some(c), None) => Ok(Some((c, ProfileRefOrigin::ConfigToml))),
        (None, Some(r)) => Ok(Some((r, ProfileRefOrigin::Registry))),
        (None, None) => Ok(None),
    }
}

/// Resolve the effective index profile for the library `cfg` serves.
///
/// `Ok(None)` when neither `config.toml` nor the registry entry
/// references a profile — the library then runs on field-level
/// configuration alone. Errors when the two references disagree, when
/// the referenced profile does not resolve, when the profile's model
/// contradicts an explicit `embed_model` in `config.toml`
/// ([`ConfigError::ProfileConfigConflict`]), or when the profile enables
/// the not-yet-implemented reranker stage.
pub fn effective_index_profile(cfg: &Config) -> Result<Option<EffectiveProfile>> {
    let config_ref = cfg.root_config().index_profile.clone();
    let registry_ref = registry_profile_ref(cfg);
    let Some((name, origin)) = effective_reference(config_ref, registry_ref)? else {
        return Ok(None);
    };

    let dir = user_profile_dir_or_default();
    let profile = resolve(&dir, &name)
        .map_err(|e| eyre!("index profile '{name}' failed to load: {e}"))?
        .ok_or_else(|| {
            eyre!(
                "index profile '{name}' is not defined; \
                 `bookrack index-profile list` shows the available names"
            )
        })?;

    if let Some(explicit) = cfg.root_config().embed_model.as_deref()
        && explicit != profile.embed.model
    {
        return Err(
            ConfigError::profile_model_conflict(&name, &profile.embed.model, explicit).into(),
        );
    }
    ensure_reranker_supported(&profile)?;

    Ok(Some(EffectiveProfile { origin, profile }))
}

/// The [`EmbedConfig`] for `cfg`'s library with every layer of the model
/// chain applied: env var > explicit `config.toml` field > the effective
/// profile's model > hardcoded default. Fails when the profile layer is
/// itself in conflict, so a handler cannot embed under a configuration
/// the daemon would refuse to start with.
pub fn effective_embed_config(cfg: &Config) -> Result<EmbedConfig> {
    let effective = effective_index_profile(cfg)?;
    let model = effective.as_ref().map(|e| e.profile.embed.model.as_str());
    Ok(EmbedConfig::resolve(cfg.root_config(), model))
}

/// Write gate for an `index_profile` reference: the name must resolve
/// and pass static validation. Returns the human-readable refusal when
/// it does not; the caller maps it to its user-error exit path. Stamp
/// consistency is deliberately not checked — reconciling a valid
/// profile against a built index is `index-profile apply`'s job.
pub fn refuse_bad_profile_reference(name: &str) -> Option<String> {
    let dir = user_profile_dir_or_default();
    match resolve(&dir, name) {
        Err(e) => Some(format!("index profile '{name}' failed to load: {e}")),
        Ok(None) => Some(format!(
            "index profile '{name}' is not defined; \
             `bookrack index-profile list` shows the available names"
        )),
        Ok(Some(profile)) => has_errors(&validate(&profile, false)).then(|| {
            format!(
                "index profile '{name}' has validation errors; \
                 run `bookrack index-profile validate {name}`"
            )
        }),
    }
}

/// The built index stamps relevant to a profile: the embed model and the
/// vector dimension recorded in the corpus, or `None` when the corpus is
/// missing, unbuilt, or cannot be opened (so a check skips rather than
/// racing a live writer).
pub fn built_stamps(data_dir: &Path) -> Option<(String, u32)> {
    let corpus = Corpus::open_read_only(&data_dir.join("corpus.db")).ok()?;
    let model = corpus.meta_get(EMBED_MODEL_KEY).ok()??;
    let dim = corpus.meta_get(VECTOR_DIM_KEY).ok()??.parse::<u32>().ok()?;
    Some((model, dim))
}

/// Field-level mismatches between a profile's embed contract and the
/// built index stamps. Empty means consistent.
pub fn profile_stamp_findings(profile: &IndexProfile, built: &(String, u32)) -> Vec<String> {
    let (built_model, built_dim) = built;
    let mut findings = Vec::new();
    if *built_model != profile.embed.model {
        findings.push(format!(
            "embed.model: profile declares '{}' but the built index is stamped '{built_model}'",
            profile.embed.model
        ));
    }
    if *built_dim != profile.embed.dim {
        findings.push(format!(
            "embed.dim: profile declares {} but the built index is stamped {built_dim}",
            profile.embed.dim
        ));
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_index_profile::PROFILE_QWEN3_06B_DEFAULT;

    #[test]
    fn effective_reference_covers_all_declaration_shapes() {
        // Nothing declared: no profile in effect.
        assert!(effective_reference(None, None).expect("ok").is_none());

        // One side declared: that side is the origin.
        let (name, origin) = effective_reference(Some("p".to_string()), None)
            .expect("ok")
            .expect("some");
        assert_eq!(name, "p");
        assert_eq!(origin, ProfileRefOrigin::ConfigToml);
        let (name, origin) = effective_reference(None, Some("p".to_string()))
            .expect("ok")
            .expect("some");
        assert_eq!(name, "p");
        assert_eq!(origin, ProfileRefOrigin::Registry);

        // Both agreeing: allowed, and marked as such.
        let (_, origin) = effective_reference(Some("p".to_string()), Some("p".to_string()))
            .expect("ok")
            .expect("some");
        assert_eq!(origin, ProfileRefOrigin::Both);

        // Both disagreeing: the conflict error, neither side preferred.
        let err = effective_reference(Some("a".to_string()), Some("b".to_string()))
            .expect_err("conflict");
        assert!(matches!(err, ConfigError::ProfileConfigConflict { .. }));
    }

    #[test]
    fn stamp_findings_report_each_divergent_field() {
        let profile = IndexProfile::from_named(PROFILE_QWEN3_06B_DEFAULT).expect("built-in");
        let matching = (profile.embed.model.clone(), profile.embed.dim);
        assert!(profile_stamp_findings(&profile, &matching).is_empty());

        let wrong_model = ("other-model".to_string(), profile.embed.dim);
        let findings = profile_stamp_findings(&profile, &wrong_model);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("embed.model"));

        let wrong_both = ("other-model".to_string(), profile.embed.dim + 1);
        assert_eq!(profile_stamp_findings(&profile, &wrong_both).len(), 2);
    }
}
