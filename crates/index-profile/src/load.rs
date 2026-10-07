// SPDX-License-Identifier: Apache-2.0

//! Parsing a profile file into an [`IndexProfile`]. Unlike a library
//! manifest — which stays forward-compatible so an old binary tolerates a
//! newer file — a profile uses `deny_unknown_fields`: a misspelled key in
//! a combination rule must fail loudly, never be silently ignored.
//!
//! The `schema_version` gate runs the same direction as the manifest's:
//! this binary reads its own schema and every earlier one, and refuses
//! a file from a later one. A bump that changes the shape ships a reader
//! for the previous schema, or a rewrite command, in the same change.

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
    /// The file declares a `schema_version` above the one this binary
    /// reads.
    #[error(
        "index profile at {path} declares schema_version {found}, newer than the {SCHEMA_VERSION} this binary reads"
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
    // A file from an earlier schema is this binary's to read: a bump
    // that changes the shape ships a reader for the previous one, or a
    // rewrite command, in the same change (`docs/UPGRADE.md`). A file
    // from a later schema is refused, naming both versions.
    if file.schema_version > SCHEMA_VERSION {
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
    fn rejects_the_hnsw_graph_keys_the_ann_spec_omits() {
        // The AnnSpec doc promises a profile naming an HNSW graph
        // parameter (`m`, `ef`) is rejected, because the vector store
        // does not expose them.
        for key in ["m", "ef"] {
            let toml = format!(
                "schema_version = 1\nname = \"x\"\n\
                 [embed]\nbackend = \"ollama\"\nmodel = \"m\"\ndim = 8\n\
                 [ann]\nkind = \"ivf-hnsw-sq\"\nnum_partitions = 1\nnprobes = 1\n{key} = 16\n"
            );
            let err = parse_str(&toml, "x").expect_err("graph key rejected");
            match err {
                super::ProfileLoadError::Parse { reason, .. } => assert!(
                    reason.contains(&format!("`{key}`")),
                    "error names the offending key: {reason}",
                ),
                other => panic!("expected Parse for `{key}`, got {other:?}"),
            }
        }
    }

    /// The loader reads its own schema and every earlier one; a bump
    /// that changes the shape ships a reader for the previous schema in
    /// the same change, so a file written before it still loads.
    #[test]
    fn a_profile_from_an_earlier_schema_still_loads() {
        let toml = format!(
            "schema_version = {}\nname = \"x\"\n\
             [embed]\nbackend = \"ollama\"\nmodel = \"m\"\ndim = 8\n\
             [ann]\nkind = \"brute-force\"\nnum_partitions = 1\nnprobes = 1\n",
            super::SCHEMA_VERSION - 1
        );
        let profile = parse_str(&toml, "x").expect("an earlier schema is read");
        assert_eq!(profile.name, "x");
    }

    #[test]
    fn rejects_a_profile_from_a_newer_schema() {
        let toml = "schema_version = 99\nname = \"x\"\n\
                    [embed]\nbackend = \"ollama\"\nmodel = \"m\"\ndim = 8\n\
                    [ann]\nkind = \"brute-force\"\nnum_partitions = 1\nnprobes = 1\n";
        let err = parse_str(toml, "x").expect_err("schema version rejected");
        assert!(matches!(
            err,
            super::ProfileLoadError::SchemaVersion { found: 99, .. }
        ));
        let rendered = err.to_string();
        assert!(
            rendered.contains("newer than"),
            "the refusal says which direction it is: {rendered}"
        );
    }
}
