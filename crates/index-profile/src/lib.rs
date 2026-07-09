// SPDX-License-Identifier: Apache-2.0

//! Index profiles: a named, statically-checkable atom that couples the
//! three retrieval knobs — the embedding model, the ANN index shape, and
//! the reranker stage — so their combination constraints are validated
//! up front instead of surfacing as a stamp mismatch or an index-build
//! error after the fact.
//!
//! This crate is the read side: it carries the built-in presets compiled
//! into the binary, loads a user-authored profile from a directory, and
//! runs the static validator. Applying a profile to a library (the write
//! side) lives elsewhere.
//!
//! A profile is "built-in preset plus user override", mirroring
//! `bookrack_audit_profile`: a user file under the per-user profile
//! directory shadows a built-in of the same name.

use std::path::{Path, PathBuf};

mod load;
mod models;
mod validate;

pub use load::{ProfileLoadError, parse_str};
pub use models::{
    EMBED_MODELS, EmbedModelInfo, RERANKER_MODELS, RerankerModelInfo, embed_model, reranker_model,
};
pub use validate::{Finding, Severity, has_errors, validate};

/// Schema version every profile file must declare. Bumped only when a
/// renamed or removed field changes an existing file's on-disk meaning.
pub const SCHEMA_VERSION: u32 = 1;

/// File extension for a user profile, appended to the profile name.
pub const PROFILE_FILE_EXT: &str = "toml";

/// Directory (under the per-user config root, beside `registry.toml`)
/// that holds user-authored profiles, one `<name>.toml` per profile.
pub const USER_PROFILE_DIR_NAME: &str = "index-profiles";

/// Built-in profile name: the small, default embedding model with a
/// product-quantized IVF index and no reranker.
pub const PROFILE_QWEN3_06B_DEFAULT: &str = "qwen3-0.6b-default";

/// Built-in profile name: the larger embedding model with an HNSW index
/// and a cross-encoder reranker stage.
pub const PROFILE_QWEN3_4B_QUALITY: &str = "qwen3-4b-quality";

/// Every built-in profile name, in listing order.
pub const ALL_BUILT_IN_NAMES: &[&str] = &[PROFILE_QWEN3_06B_DEFAULT, PROFILE_QWEN3_4B_QUALITY];

/// The TOML source of the `qwen3-0.6b-default` built-in profile.
pub const QWEN3_06B_DEFAULT_TOML: &str = include_str!("../data/qwen3-0.6b-default.toml");

/// The TOML source of the `qwen3-4b-quality` built-in profile.
pub const QWEN3_4B_QUALITY_TOML: &str = include_str!("../data/qwen3-4b-quality.toml");

/// Which IVF index family the ANN store builds. String forms mirror the
/// kebab-case labels the `vectors` crate persists and parses, so a
/// profile's `kind` compares directly against a built index's recorded
/// kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnnKind {
    /// Flat IVF: exact vectors per partition.
    IvfFlat,
    /// Scalar-quantized IVF.
    IvfSq,
    /// Product-quantized IVF.
    IvfPq,
    /// HNSW graph over flat vectors.
    IvfHnswFlat,
    /// HNSW graph over scalar-quantized vectors.
    IvfHnswSq,
    /// HNSW graph over product-quantized vectors.
    IvfHnswPq,
    /// No ANN index; exhaustive scan.
    BruteForce,
}

impl AnnKind {
    /// The kebab-case label, matching the `vectors` crate's persisted
    /// form.
    pub fn as_str(self) -> &'static str {
        match self {
            AnnKind::IvfFlat => "ivf-flat",
            AnnKind::IvfSq => "ivf-sq",
            AnnKind::IvfPq => "ivf-pq",
            AnnKind::IvfHnswFlat => "ivf-hnsw-flat",
            AnnKind::IvfHnswSq => "ivf-hnsw-sq",
            AnnKind::IvfHnswPq => "ivf-hnsw-pq",
            AnnKind::BruteForce => "brute-force",
        }
    }

    /// True for the product-quantized families that require and constrain
    /// `num_sub_vectors`.
    pub fn is_pq(self) -> bool {
        matches!(self, AnnKind::IvfPq | AnnKind::IvfHnswPq)
    }

    /// True for the HNSW families that carry an upstream recall
    /// regression on the pinned LanceDB.
    pub fn is_hnsw(self) -> bool {
        matches!(
            self,
            AnnKind::IvfHnswFlat | AnnKind::IvfHnswSq | AnnKind::IvfHnswPq
        )
    }
}

/// The reranker stage kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RerankerKind {
    /// No reranking; the ANN ranking is final.
    #[default]
    None,
    /// A cross-encoder scores the top candidates against the query.
    CrossEncoder,
}

/// The embedding knob: which model produces the vectors and the
/// dimension they carry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedSpec {
    /// Embedding backend. Currently only `ollama`.
    pub backend: String,
    /// Embedding model tag.
    pub model: String,
    /// Vector dimension the model emits.
    pub dim: u32,
}

/// The ANN knob. Field names mirror `vectors`' `AnnConfig`; there are no
/// HNSW graph parameters (`m`, `ef`) because the vector store does not
/// expose them, so a profile that names one is rejected.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnSpec {
    /// IVF family.
    pub kind: AnnKind,
    /// `k` for the IVF k-means quantizer.
    pub num_partitions: u32,
    /// PQ sub-vector count; required for the PQ families.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_sub_vectors: Option<u32>,
    /// PQ code width in bits per sub-vector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_bits: Option<u32>,
    /// Query-time partition fan-out.
    pub nprobes: u32,
    /// Query-time refinement multiplier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refine_factor: Option<u32>,
}

/// The reranker knob. Every field beyond `kind` is optional at the type
/// level and required by the validator when `kind` is not `none`.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RerankerSpec {
    /// Reranker stage kind. Defaults to `none`.
    #[serde(default)]
    pub kind: RerankerKind,
    /// Reranker backend. Must not be `ollama`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Reranker model identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Candidates passed into the reranker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k_in: Option<u32>,
    /// Candidates kept after reranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k_out: Option<u32>,
}

/// A resolved index profile: the three knobs plus its identity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IndexProfile {
    /// Profile name.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Embedding knob.
    pub embed: EmbedSpec,
    /// ANN knob.
    pub ann: AnnSpec,
    /// Reranker knob.
    pub reranker: RerankerSpec,
}

impl IndexProfile {
    /// Parse a built-in profile by name, or `None` when the name is not a
    /// built-in. A built-in that fails to parse is a bug in the shipped
    /// data, so the parse is unwrapped.
    pub fn from_named(name: &str) -> Option<IndexProfile> {
        let toml = match name {
            PROFILE_QWEN3_06B_DEFAULT => QWEN3_06B_DEFAULT_TOML,
            PROFILE_QWEN3_4B_QUALITY => QWEN3_4B_QUALITY_TOML,
            _ => return None,
        };
        Some(parse_str(toml, name).expect("built-in profile parses"))
    }
}

/// The verbatim TOML source of a built-in profile, or `None` when the
/// name is not a built-in. Lets `show` print a profile's source exactly
/// as shipped.
pub fn builtin_toml(name: &str) -> Option<&'static str> {
    match name {
        PROFILE_QWEN3_06B_DEFAULT => Some(QWEN3_06B_DEFAULT_TOML),
        PROFILE_QWEN3_4B_QUALITY => Some(QWEN3_4B_QUALITY_TOML),
        _ => None,
    }
}

/// Where a resolved profile came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileOrigin {
    /// Compiled into the binary.
    BuiltIn,
    /// Loaded from the user profile directory.
    User,
}

/// A profile paired with its origin, for listing and shadow reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    /// The profile name.
    pub name: String,
    /// Where it was resolved from.
    pub origin: ProfileOrigin,
    /// True when a user profile shadows a built-in of the same name.
    pub shadows_builtin: bool,
}

/// The path a user profile named `name` would occupy under `dir`.
pub fn user_profile_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{PROFILE_FILE_EXT}"))
}

/// Resolve a profile by name: a user file under `dir` wins over a
/// built-in of the same name. Returns `Ok(None)` when neither defines the
/// name.
pub fn resolve(dir: &Path, name: &str) -> Result<Option<IndexProfile>, ProfileLoadError> {
    let path = user_profile_path(dir, name);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(parse_str(&text, &path.to_string_lossy())?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IndexProfile::from_named(name)),
        Err(source) => Err(ProfileLoadError::Io {
            path: path.to_string_lossy().into_owned(),
            reason: source.to_string(),
        }),
    }
}

/// List every profile name visible under `dir`, built-ins merged with the
/// user directory, sorted, each marked with its origin and whether a user
/// file shadows a built-in. A user directory that cannot be read is
/// treated as empty — only the built-ins are listed.
pub fn list_profiles(dir: &Path) -> Vec<ProfileEntry> {
    let mut user_names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some(PROFILE_FILE_EXT)
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                user_names.push(stem.to_string());
            }
        }
    }

    let mut names: Vec<String> = ALL_BUILT_IN_NAMES.iter().map(|s| s.to_string()).collect();
    for name in &user_names {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names.sort();

    names
        .into_iter()
        .map(|name| {
            let is_builtin = ALL_BUILT_IN_NAMES.contains(&name.as_str());
            let is_user = user_names.contains(&name);
            let origin = if is_user {
                ProfileOrigin::User
            } else {
                ProfileOrigin::BuiltIn
            };
            ProfileEntry {
                name,
                origin,
                shadows_builtin: is_user && is_builtin,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_names_resolve_and_validate_clean() {
        // The "built-in is exemplary" guard: every shipped profile must
        // parse and validate with no Error findings. Warnings (HNSW)
        // are allowed.
        for name in ALL_BUILT_IN_NAMES {
            let profile = IndexProfile::from_named(name).expect("built-in resolves");
            let findings = validate(&profile, false);
            let errors: Vec<_> = findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .collect();
            assert!(
                errors.is_empty(),
                "built-in {name} has validation errors: {errors:?}",
            );
        }
    }

    #[test]
    fn quality_built_in_declares_the_live_reranker_stage() {
        // The quality profile's reranker section is an executable
        // combination: the implemented backend, a registry model tag,
        // and the candidate window the query path applies.
        let quality = IndexProfile::from_named(PROFILE_QWEN3_4B_QUALITY).expect("built-in");
        assert_eq!(quality.reranker.kind, RerankerKind::CrossEncoder);
        assert_eq!(quality.reranker.backend.as_deref(), Some("llama-server"));
        assert_eq!(
            quality.reranker.model.as_deref(),
            Some("Qwen3-Reranker-0.6B")
        );
        assert_eq!(quality.reranker.top_k_in, Some(50));
        assert_eq!(quality.reranker.top_k_out, Some(10));
    }

    #[test]
    fn from_named_rejects_an_unknown_name() {
        assert!(IndexProfile::from_named("no-such-profile").is_none());
    }

    #[test]
    fn user_profile_shadows_a_builtin_of_the_same_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml = QWEN3_06B_DEFAULT_TOML.replace("dim = 1024", "dim = 2048");
        std::fs::write(
            user_profile_path(dir.path(), PROFILE_QWEN3_06B_DEFAULT),
            toml,
        )
        .expect("write user profile");

        let resolved = resolve(dir.path(), PROFILE_QWEN3_06B_DEFAULT)
            .expect("resolves")
            .expect("some profile");
        assert_eq!(resolved.embed.dim, 2048, "user file should win");

        let listed = list_profiles(dir.path());
        let shadowed = listed
            .iter()
            .find(|e| e.name == PROFILE_QWEN3_06B_DEFAULT)
            .expect("listed");
        assert_eq!(shadowed.origin, ProfileOrigin::User);
        assert!(shadowed.shadows_builtin);
    }

    #[test]
    fn resolve_returns_none_for_an_unknown_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(resolve(dir.path(), "nope").expect("resolves").is_none());
    }
}
