// SPDX-License-Identifier: Apache-2.0

//! Runtime data set the papers pipeline's audit consults.
//!
//! Loaded from a schema-locked default embedded at build time plus an
//! optional overlay under `<data_root>/audit-rules/`. Mirrors the
//! books pipeline's `bookrack-audit-profile` data crate, with lists
//! tuned for paper signals: venue whitelist, venue aliases,
//! placeholder titles, watermark tokens, sentinel contributor names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File name of the runtime overlay.
pub const DATA_OVERLAY_FILE: &str = "paper_audit_data.local.toml";

/// Schema version the loader accepts.
pub const SCHEMA_VERSION: u32 = 1;

/// In-repo default data source, embedded at build time.
pub const DEFAULT_DATA_TOML: &str = include_str!("../../data/paper_audit_data.toml");

/// Runtime data set consumed by the audit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaperAuditData {
    /// Container titles the audit recognises as established venues. A
    /// miss against this list downgrades `container_title` to Weak
    /// but never floors the verdict.
    pub venue_whitelist: Vec<String>,
    /// Map of informal-to-canonical venue spellings.
    pub venue_aliases: BTreeMap<String, String>,
    /// Titles whose normalised form matches one of these are graded
    /// Missing rather than Strong.
    pub placeholder_titles: Vec<String>,
    /// Tokens whose presence in a field value flags
    /// `SourceWatermark`.
    pub watermark_tokens: Vec<String>,
    /// Contributor names treated as sentinels rather than real
    /// authors.
    pub sentinel_contributor_names: Vec<String>,
}

impl PaperAuditData {
    /// The shipped `default_data`, parsed from
    /// [`DEFAULT_DATA_TOML`].
    pub fn default_data() -> Self {
        parse_str(DEFAULT_DATA_TOML).expect("shipped default paper_audit_data.toml must parse")
    }

    /// An empty data set: every list empty. Useful in tests and when
    /// an operator wants to disable every list-driven signal without
    /// touching the profile.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load from disk: parse the embedded default first, then merge
    /// the overlay at `<dir>/paper_audit_data.local.toml` on top. A
    /// missing overlay yields the shipped default.
    pub fn load_from(dir: &Path) -> Result<Self, DataLoadError> {
        let mut data = Self::default_data();
        let overlay_path = dir.join(DATA_OVERLAY_FILE);
        if overlay_path.exists() {
            let raw =
                std::fs::read_to_string(&overlay_path).map_err(|error| DataLoadError::Io {
                    path: overlay_path.clone(),
                    error,
                })?;
            merge_overlay(&mut data, &raw, &overlay_path)?;
        }
        Ok(data)
    }
}

/// Reasons a load can fail.
#[derive(Debug)]
pub enum DataLoadError {
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    Parse {
        path: PathBuf,
        error: toml::de::Error,
    },
    SchemaVersion {
        path: PathBuf,
        found: u32,
    },
}

impl std::fmt::Display for DataLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, error } => write!(f, "failed to read {}: {error}", path.display()),
            Self::Parse { path, error } => {
                write!(f, "failed to parse {}: {error}", path.display())
            }
            Self::SchemaVersion { path, found } => write!(
                f,
                "unsupported schema_version {found} in {} (expected {SCHEMA_VERSION})",
                path.display()
            ),
        }
    }
}

impl std::error::Error for DataLoadError {}

fn parse_str(raw: &str) -> Result<PaperAuditData, DataLoadError> {
    let synthetic = PathBuf::from("<embedded:paper_audit_data>");
    let file: DataFile = toml::from_str(raw).map_err(|error| DataLoadError::Parse {
        path: synthetic.clone(),
        error,
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(DataLoadError::SchemaVersion {
            path: synthetic,
            found: file.schema_version,
        });
    }
    let mut data = PaperAuditData::default();
    apply_overlay(&mut data, file);
    Ok(data)
}

fn merge_overlay(data: &mut PaperAuditData, raw: &str, path: &Path) -> Result<(), DataLoadError> {
    let file: DataFile = toml::from_str(raw).map_err(|error| DataLoadError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(DataLoadError::SchemaVersion {
            path: path.to_path_buf(),
            found: file.schema_version,
        });
    }
    apply_overlay(data, file);
    Ok(())
}

fn apply_overlay(data: &mut PaperAuditData, file: DataFile) {
    if let Some(v) = file.venue_whitelist {
        data.venue_whitelist = v;
    }
    if let Some(v) = file.venue_aliases {
        data.venue_aliases = v;
    }
    if let Some(v) = file.placeholder_titles {
        data.placeholder_titles = v;
    }
    if let Some(v) = file.watermark_tokens {
        data.watermark_tokens = v;
    }
    if let Some(v) = file.sentinel_contributor_names {
        data.sentinel_contributor_names = v;
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DataFile {
    schema_version: u32,
    #[serde(default)]
    venue_whitelist: Option<Vec<String>>,
    #[serde(default)]
    venue_aliases: Option<BTreeMap<String, String>>,
    #[serde(default)]
    placeholder_titles: Option<Vec<String>>,
    #[serde(default)]
    watermark_tokens: Option<Vec<String>>,
    #[serde(default)]
    sentinel_contributor_names: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_data_parses_and_seeds_lists() {
        let d = PaperAuditData::default_data();
        assert!(d.venue_whitelist.is_empty());
        assert!(d.venue_aliases.is_empty());
        assert!(d.placeholder_titles.contains(&"untitled".to_string()));
        assert!(d.watermark_tokens.contains(&"scribd".to_string()));
        assert!(
            d.sentinel_contributor_names
                .iter()
                .any(|n| n == "Editorial Board")
        );
    }

    #[test]
    fn empty_data_has_every_list_empty() {
        let d = PaperAuditData::empty();
        assert!(d.venue_whitelist.is_empty());
        assert!(d.placeholder_titles.is_empty());
        assert!(d.watermark_tokens.is_empty());
        assert!(d.sentinel_contributor_names.is_empty());
    }

    #[test]
    fn load_from_missing_dir_returns_default_data() {
        let dir = TempDir::new().unwrap();
        let d = PaperAuditData::load_from(dir.path()).unwrap();
        assert_eq!(d, PaperAuditData::default_data());
    }

    #[test]
    fn overlay_replaces_lists_field_by_field() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(DATA_OVERLAY_FILE);
        std::fs::write(
            &overlay,
            "schema_version = 1\n\
             venue_whitelist = [\"Nature\", \"Science\"]\n",
        )
        .unwrap();
        let d = PaperAuditData::load_from(dir.path()).unwrap();
        assert_eq!(d.venue_whitelist, vec!["Nature", "Science"]);
        // Lists not declared in the overlay keep their default value.
        assert!(d.placeholder_titles.contains(&"untitled".to_string()));
    }

    #[test]
    fn overlay_with_wrong_schema_version_fails() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(DATA_OVERLAY_FILE);
        std::fs::write(&overlay, "schema_version = 99\n").unwrap();
        let err = PaperAuditData::load_from(dir.path()).unwrap_err();
        assert!(matches!(err, DataLoadError::SchemaVersion { .. }));
    }
}
