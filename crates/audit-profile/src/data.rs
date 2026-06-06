// SPDX-License-Identifier: Apache-2.0

//! Runtime-loaded data lists the metadata audit, the diagnose
//! scrubber, and the ingest dryrun walker consume.
//!
//! Two sources feed [`AuditData::load_from`]:
//!
//! 1. The crate ships a schema-locked default at
//!    `crates/audit-profile/data/audit_data.toml`, embedded through
//!    `include_str!` so an install needs no on-disk file to start.
//!    The default reproduces the previously hard-coded URL / email /
//!    abbreviation / placeholder / extension lists, with the
//!    operator-curated token lists (whitelist, watermark tokens, volume
//!    suffixes) shipped empty.
//! 2. An optional overlay at
//!    `<data_root>/audit-rules/audit_data.toml`. Each field present
//!    REPLACES the shipped default; missing fields fall through.
//!    A malformed file aborts the load.
//!
//! This file consolidates what previously lived in two separate files
//! (`publishers.toml` + `watermarks.toml`) plus a handful of inline
//! constants scattered across `metadata`, `ingest`, and `diagnose`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File name of the runtime overlay, looked up under
/// `<data_root>/audit-rules/`.
pub const DATA_OVERLAY_FILE: &str = "audit_data.toml";
/// Schema version the loader accepts.
pub const DATA_SCHEMA_VERSION: u32 = 1;

/// In-repo default data source, embedded at build time.
pub const DEFAULT_DATA_TOML: &str = include_str!("../data/audit_data.toml");

/// Data lists the audit and adjacent stages consult.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditData {
    /// Reputable-imprint whitelist.
    pub publisher_whitelist: Vec<String>,
    /// Closed-form URL substrings the watermark sniffer matches.
    pub watermark_url_substrings: Vec<String>,
    /// Closed-form e-mail substrings the watermark sniffer matches.
    pub watermark_email_substrings: Vec<String>,
    /// Tokens that mark contact / chat channels.
    pub contact_tokens: Vec<String>,
    /// Tokens that mark promotional / distribution verbs.
    pub promo_tokens: Vec<String>,
    /// ASCII brand handles of known distribution channels.
    pub ascii_distribution_tokens: Vec<String>,
    /// CJK fragments that mark watermarks or distribution brands.
    pub watermark_cjk_tokens: Vec<String>,
    /// `token -> expansion` map applied during whitelist normalisation.
    pub abbreviations: BTreeMap<String, String>,
    /// Case-insensitive placeholder-title words the title audit flags.
    pub placeholder_titles: Vec<String>,
    /// Suffix tokens that mark a trailing bracketed segment as a
    /// volume / edition / printing marker.
    pub volume_suffix_tokens: Vec<String>,
    /// Extensions the ingest dryrun walks under a directory.
    pub book_extensions: Vec<String>,
    /// Extensions whose basenames the diagnose scrubber hashes.
    pub scrub_book_extensions: Vec<String>,
}

impl AuditData {
    /// The shipped default data set: reproduces the pre-consolidation
    /// hard-coded behaviour field-for-field.
    pub fn default_data() -> Self {
        parse_str(DEFAULT_DATA_TOML, &PathBuf::from("<embedded:audit_data>"))
            .expect("shipped default audit_data.toml must parse")
    }

    /// An empty data set; every list-driven signal then falls through
    /// to neutral. Useful in tests.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load the data from disk. The schema-locked default is parsed
    /// first; an optional overlay at `<dir>/audit_data.toml` is then
    /// merged on top. A missing directory or missing overlay yields
    /// the shipped default; a malformed overlay returns an error.
    pub fn load_from(dir: &Path) -> Result<Self, DataLoadError> {
        if !dir.exists() {
            return Ok(Self::default_data());
        }
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

/// Reasons a data load can fail.
#[derive(Debug)]
pub enum DataLoadError {
    /// The overlay existed but could not be read.
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    /// The overlay was read but did not parse as the expected schema.
    Parse {
        path: PathBuf,
        error: toml::de::Error,
    },
    /// A file's `schema_version` was not the supported value.
    SchemaVersion { path: PathBuf, found: u32 },
}

impl std::fmt::Display for DataLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, error } => {
                write!(f, "failed to read {}: {error}", path.display())
            }
            Self::Parse { path, error } => {
                write!(f, "failed to parse {}: {error}", path.display())
            }
            Self::SchemaVersion { path, found } => write!(
                f,
                "unsupported schema_version {found} in {} (expected {DATA_SCHEMA_VERSION})",
                path.display()
            ),
        }
    }
}

impl std::error::Error for DataLoadError {}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DataFile {
    schema_version: u32,
    #[serde(default)]
    publishers: Option<PublishersSection>,
    #[serde(default)]
    watermarks: Option<WatermarksSection>,
    #[serde(default)]
    abbreviations: Option<BTreeMap<String, String>>,
    #[serde(default)]
    title: Option<TitleSection>,
    #[serde(default)]
    io: Option<IoSection>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PublishersSection {
    #[serde(default)]
    whitelist: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct WatermarksSection {
    #[serde(default)]
    url_substrings: Option<Vec<String>>,
    #[serde(default)]
    email_substrings: Option<Vec<String>>,
    #[serde(default)]
    contact_tokens: Option<Vec<String>>,
    #[serde(default)]
    promo_tokens: Option<Vec<String>>,
    #[serde(default)]
    ascii_distribution_tokens: Option<Vec<String>>,
    #[serde(default)]
    cjk_tokens: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TitleSection {
    #[serde(default)]
    placeholder_words: Option<Vec<String>>,
    #[serde(default)]
    volume_suffix_tokens: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct IoSection {
    #[serde(default)]
    book_extensions: Option<Vec<String>>,
    #[serde(default)]
    scrub_book_extensions: Option<Vec<String>>,
}

fn parse_str(raw: &str, path: &Path) -> Result<AuditData, DataLoadError> {
    let file: DataFile = toml::from_str(raw).map_err(|error| DataLoadError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    if file.schema_version != DATA_SCHEMA_VERSION {
        return Err(DataLoadError::SchemaVersion {
            path: path.to_path_buf(),
            found: file.schema_version,
        });
    }
    let mut data = AuditData::default();
    apply_overlay(&mut data, file);
    Ok(data)
}

fn merge_overlay(data: &mut AuditData, raw: &str, path: &Path) -> Result<(), DataLoadError> {
    let file: DataFile = toml::from_str(raw).map_err(|error| DataLoadError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    if file.schema_version != DATA_SCHEMA_VERSION {
        return Err(DataLoadError::SchemaVersion {
            path: path.to_path_buf(),
            found: file.schema_version,
        });
    }
    apply_overlay(data, file);
    Ok(())
}

fn apply_overlay(data: &mut AuditData, file: DataFile) {
    if let Some(s) = file.publishers
        && let Some(v) = s.whitelist
    {
        data.publisher_whitelist = v;
    }
    if let Some(s) = file.watermarks {
        if let Some(v) = s.url_substrings {
            data.watermark_url_substrings = v;
        }
        if let Some(v) = s.email_substrings {
            data.watermark_email_substrings = v;
        }
        if let Some(v) = s.contact_tokens {
            data.contact_tokens = v;
        }
        if let Some(v) = s.promo_tokens {
            data.promo_tokens = v;
        }
        if let Some(v) = s.ascii_distribution_tokens {
            data.ascii_distribution_tokens = v;
        }
        if let Some(v) = s.cjk_tokens {
            data.watermark_cjk_tokens = v;
        }
    }
    if let Some(v) = file.abbreviations {
        data.abbreviations = v;
    }
    if let Some(s) = file.title {
        if let Some(v) = s.placeholder_words {
            data.placeholder_titles = v;
        }
        if let Some(v) = s.volume_suffix_tokens {
            data.volume_suffix_tokens = v;
        }
    }
    if let Some(s) = file.io {
        if let Some(v) = s.book_extensions {
            data.book_extensions = v;
        }
        if let Some(v) = s.scrub_book_extensions {
            data.scrub_book_extensions = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, body: &str) {
        let mut f = std::fs::File::create(dir.join(name)).expect("create");
        f.write_all(body.as_bytes()).expect("write");
    }

    #[test]
    fn default_data_matches_shipped_defaults() {
        let data = AuditData::default_data();
        assert!(data.publisher_whitelist.is_empty());
        assert!(data.contact_tokens.is_empty());
        assert!(data.promo_tokens.is_empty());
        assert!(data.ascii_distribution_tokens.is_empty());
        assert!(data.watermark_cjk_tokens.is_empty());
        assert!(data.volume_suffix_tokens.is_empty());
        assert!(data.watermark_url_substrings.iter().any(|s| s == "http://"));
        assert!(data.watermark_url_substrings.iter().any(|s| s == "www."));
        assert!(data.watermark_email_substrings.iter().any(|s| s == "@"));
        assert_eq!(
            data.abbreviations.get("univ").map(String::as_str),
            Some("university")
        );
        assert_eq!(
            data.abbreviations.get("pub").map(String::as_str),
            Some("publishing")
        );
        assert!(data.placeholder_titles.iter().any(|w| w == "unknown"));
        assert!(data.book_extensions.iter().any(|e| e == "epub"));
        assert!(data.book_extensions.iter().any(|e| e == "html"));
        assert!(data.scrub_book_extensions.iter().any(|e| e == "pdf"));
        assert!(!data.scrub_book_extensions.iter().any(|e| e == "html"));
    }

    #[test]
    fn load_from_empty_directory_yields_default_data() {
        let dir = TempDir::new().unwrap();
        let loaded = AuditData::load_from(dir.path()).unwrap();
        assert_eq!(loaded, AuditData::default_data());
    }

    #[test]
    fn load_from_absent_directory_yields_default_data() {
        let dir = TempDir::new().unwrap();
        let absent = dir.path().join("nonexistent");
        let loaded = AuditData::load_from(&absent).unwrap();
        assert_eq!(loaded, AuditData::default_data());
    }

    #[test]
    fn overlay_replaces_named_fields_only() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            DATA_OVERLAY_FILE,
            "schema_version = 1\n\
             [publishers]\n\
             whitelist = [\"Acme University Press\"]\n\
             [watermarks]\n\
             cjk_tokens = [\"\\u638C\\u4E0A\\u4E66\\u82D1\"]\n",
        );
        let loaded = AuditData::load_from(dir.path()).unwrap();
        assert_eq!(loaded.publisher_whitelist, vec!["Acme University Press"]);
        assert_eq!(loaded.watermark_cjk_tokens.len(), 1);
        // Untouched fields still hold the shipped default.
        assert!(loaded.watermark_url_substrings.iter().any(|s| s == "www."));
        assert!(loaded.placeholder_titles.iter().any(|w| w == "anonymous"));
        assert_eq!(
            loaded.abbreviations.get("intl").map(String::as_str),
            Some("international"),
        );
    }

    #[test]
    fn overlay_can_clear_a_shipped_default_list() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            DATA_OVERLAY_FILE,
            "schema_version = 1\n[watermarks]\nurl_substrings = []\n",
        );
        let loaded = AuditData::load_from(dir.path()).unwrap();
        assert!(loaded.watermark_url_substrings.is_empty());
        // Other watermark fields keep the shipped default.
        assert!(loaded.watermark_email_substrings.iter().any(|s| s == "@"));
    }

    #[test]
    fn overlay_with_unsupported_schema_version_rejected() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), DATA_OVERLAY_FILE, "schema_version = 99\n");
        let err = AuditData::load_from(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            DataLoadError::SchemaVersion { found: 99, .. }
        ));
    }

    #[test]
    fn malformed_overlay_returns_parse_error() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), DATA_OVERLAY_FILE, "not = valid = toml\n");
        let err = AuditData::load_from(dir.path()).unwrap_err();
        assert!(matches!(err, DataLoadError::Parse { .. }));
    }
}
