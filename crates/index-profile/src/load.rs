// SPDX-License-Identifier: Apache-2.0

//! Parsing a profile file into an [`IndexProfile`]. Unlike a library
//! manifest — which stays forward-compatible so an old binary tolerates a
//! newer file — a profile uses `deny_unknown_fields`: a misspelled key in
//! a combination rule must fail loudly, never be silently ignored.

use crate::{AnnSpec, EmbedSpec, IndexProfile, RerankerSpec, SCHEMA_VERSION};

/// Why a profile file could not be turned into an [`IndexProfile`].
#[derive(Debug, thiserror::Error)]
pub enum ProfileLoadError {
    /// The file could not be read.
    #[error("cannot read index profile at {path}: {reason}")]
    Io {
        /// The file path.
        path: String,
        /// The formatted I/O error.
        reason: String,
    },
    /// The file is not valid TOML for a profile (bad syntax, a missing
    /// required field, an unknown key, or a bad enum value).
    #[error("index profile at {path} is malformed: {reason}")]
    Parse {
        /// The file path or embedded label.
        path: String,
        /// The formatted parse error.
        reason: String,
    },
    /// The file declares a `schema_version` this binary does not
    /// understand.
    #[error(
        "index profile at {path} declares schema_version {found}, but this binary understands {SCHEMA_VERSION}"
    )]
    SchemaVersion {
        /// The file path or embedded label.
        path: String,
        /// The version the file declared.
        found: u32,
    },
}

/// On-disk shape of a profile file: the wire fields plus the required
/// `schema_version`. `deny_unknown_fields` rejects a stray key here and,
/// through the specs' own attribute, in every nested section.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    schema_version: u32,
    name: String,
    #[serde(default)]
    description: String,
    embed: EmbedSpec,
    ann: AnnSpec,
    #[serde(default)]
    reranker: RerankerSpec,
}

/// Parse `toml` into an [`IndexProfile`]. `path` labels the source in any
/// error — a real filesystem path for a user file, or the profile name
/// for a built-in compiled into the binary.
pub fn parse_str(toml: &str, path: &str) -> Result<IndexProfile, ProfileLoadError> {
    let file: ProfileFile = toml::from_str(toml).map_err(|e| ProfileLoadError::Parse {
        path: path.to_string(),
        reason: e.to_string(),
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(ProfileLoadError::SchemaVersion {
            path: path.to_string(),
            found: file.schema_version,
        });
    }
    Ok(IndexProfile {
        name: file.name,
        description: file.description,
        embed: file.embed,
        ann: file.ann,
        reranker: file.reranker,
    })
}

#[cfg(test)]
mod tests {
    use crate::{AnnKind, QWEN3_06B_DEFAULT_TOML, parse_str};

    #[test]
    fn parses_a_built_in_profile() {
        let profile = parse_str(QWEN3_06B_DEFAULT_TOML, "qwen3-0.6b-default").expect("parses");
        assert_eq!(profile.name, "qwen3-0.6b-default");
        assert_eq!(profile.ann.kind, AnnKind::IvfPq);
        assert_eq!(profile.embed.dim, 1024);
    }

    #[test]
    fn rejects_an_unknown_key() {
        let toml = "schema_version = 1\nname = \"x\"\n\
                    [embed]\nbackend = \"ollama\"\nmodel = \"m\"\ndim = 8\nbogus = 1\n\
                    [ann]\nkind = \"brute-force\"\nnum_partitions = 1\nnprobes = 1\n";
        let err = parse_str(toml, "x").expect_err("unknown key rejected");
        assert!(matches!(err, super::ProfileLoadError::Parse { .. }));
    }

    #[test]
    fn rejects_a_wrong_schema_version() {
        let toml = "schema_version = 99\nname = \"x\"\n\
                    [embed]\nbackend = \"ollama\"\nmodel = \"m\"\ndim = 8\n\
                    [ann]\nkind = \"brute-force\"\nnum_partitions = 1\nnprobes = 1\n";
        let err = parse_str(toml, "x").expect_err("schema version rejected");
        assert!(matches!(
            err,
            super::ProfileLoadError::SchemaVersion { found: 99, .. }
        ));
    }
}
