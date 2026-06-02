// SPDX-License-Identifier: Apache-2.0

//! Runtime-loaded data for the audit signals.
//!
//! Two TOML files under one directory:
//!
//! - `publishers.toml` holds the reputable-imprint whitelist.
//! - `watermarks.toml` holds four token lists used by the
//!   distribution-watermark sniffer.
//!
//! Each file declares `schema_version = 1`; the loader rejects any
//! other value. Missing files yield empty sub-rules and a warning;
//! the engine then treats every value as neutral, the same outcome
//! it produced before the lists existed.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File name of the publisher whitelist on disk.
pub const PUBLISHERS_FILE: &str = "publishers.toml";
/// File name of the watermark token lists on disk.
pub const WATERMARKS_FILE: &str = "watermarks.toml";
/// Schema version every loaded file must declare.
pub const SCHEMA_VERSION: u32 = 1;

/// Loaded rule set the audit signals read at evaluation time.
#[derive(Debug, Clone, Default)]
pub struct AuditRules {
    /// Reputable-imprint whitelist, raw strings.
    pub publisher_whitelist: Vec<String>,
    /// Tokens that mark contact / chat channels.
    pub contact_tokens: Vec<String>,
    /// Tokens that mark promotional / distribution verbs.
    pub promo_tokens: Vec<String>,
    /// ASCII brand handles of known distribution channels.
    pub ascii_distribution_tokens: Vec<String>,
    /// CJK fragments that mark watermarks or distribution brands.
    pub watermark_cjk_tokens: Vec<String>,
}

impl AuditRules {
    /// An empty rule set. Every signal that consults the lists falls
    /// through to neutral.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load both files from one directory. A missing file is logged
    /// once and its sub-rules stay empty; a malformed file aborts the
    /// load with an error.
    pub fn load_from(dir: &Path) -> Result<Self, LoadError> {
        let publishers = load_publishers(&dir.join(PUBLISHERS_FILE))?;
        let watermarks = load_watermarks(&dir.join(WATERMARKS_FILE))?;
        Ok(Self {
            publisher_whitelist: publishers,
            contact_tokens: watermarks.contact_tokens,
            promo_tokens: watermarks.promo_tokens,
            ascii_distribution_tokens: watermarks.ascii_distribution_tokens,
            watermark_cjk_tokens: watermarks.watermark_cjk_tokens,
        })
    }
}

/// Reasons a load can fail.
#[derive(Debug)]
pub enum LoadError {
    /// The file existed but could not be read.
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    /// The file was read but did not parse as the expected schema.
    Parse {
        path: PathBuf,
        error: toml::de::Error,
    },
    /// The file's `schema_version` was not the supported value.
    SchemaVersion { path: PathBuf, found: u32 },
}

impl std::fmt::Display for LoadError {
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
                "unsupported schema_version {found} in {} (expected {SCHEMA_VERSION})",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LoadError {}

#[derive(Debug, Deserialize)]
struct PublishersFile {
    schema_version: u32,
    #[serde(default)]
    whitelist: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WatermarksFile {
    schema_version: u32,
    #[serde(default)]
    contact_tokens: Vec<String>,
    #[serde(default)]
    promo_tokens: Vec<String>,
    #[serde(default)]
    ascii_distribution_tokens: Vec<String>,
    #[serde(default)]
    watermark_cjk_tokens: Vec<String>,
}

fn load_publishers(path: &Path) -> Result<Vec<String>, LoadError> {
    let Some(text) = read_optional(path)? else {
        return Ok(Vec::new());
    };
    let file: PublishersFile = toml::from_str(&text).map_err(|error| LoadError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    check_schema_version(path, file.schema_version)?;
    Ok(file.whitelist)
}

fn load_watermarks(path: &Path) -> Result<WatermarksFile, LoadError> {
    let Some(text) = read_optional(path)? else {
        return Ok(WatermarksFile::default());
    };
    let file: WatermarksFile = toml::from_str(&text).map_err(|error| LoadError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    check_schema_version(path, file.schema_version)?;
    Ok(file)
}

/// Read a file if it exists; return `None` and emit a warning when
/// missing, propagate any other I/O error.
fn read_optional(path: &Path) -> Result<Option<String>, LoadError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                path = %path.display(),
                "audit rules file not found; falling back to empty list",
            );
            Ok(None)
        }
        Err(error) => Err(LoadError::Io {
            path: path.to_path_buf(),
            error,
        }),
    }
}

fn check_schema_version(path: &Path, found: u32) -> Result<(), LoadError> {
    if found == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(LoadError::SchemaVersion {
            path: path.to_path_buf(),
            found,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    fn write(dir: &Path, name: &str, body: &str) {
        let mut f = std::fs::File::create(dir.join(name)).expect("create");
        f.write_all(body.as_bytes()).expect("write");
    }

    #[test]
    fn load_returns_empty_when_both_files_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rules = AuditRules::load_from(tmp.path()).expect("load");
        assert!(rules.publisher_whitelist.is_empty());
        assert!(rules.contact_tokens.is_empty());
    }

    #[test]
    fn load_parses_both_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            tmp.path(),
            PUBLISHERS_FILE,
            "schema_version = 1\nwhitelist = [\"Acme University Press\"]\n",
        );
        write(
            tmp.path(),
            WATERMARKS_FILE,
            "schema_version = 1\n\
             contact_tokens = [\"qq:\"]\n\
             promo_tokens = [\"free ebook\"]\n\
             ascii_distribution_tokens = [\"cj5\"]\n\
             watermark_cjk_tokens = [\"\\u638C\\u4E0A\\u4E66\\u82D1\"]\n",
        );
        let rules = AuditRules::load_from(tmp.path()).expect("load");
        assert_eq!(rules.publisher_whitelist, vec!["Acme University Press"]);
        assert_eq!(rules.contact_tokens, vec!["qq:"]);
        assert_eq!(rules.promo_tokens, vec!["free ebook"]);
        assert_eq!(rules.ascii_distribution_tokens, vec!["cj5"]);
        assert_eq!(rules.watermark_cjk_tokens.len(), 1);
    }

    #[test]
    fn load_rejects_unsupported_schema_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), PUBLISHERS_FILE, "schema_version = 999\n");
        let err = AuditRules::load_from(tmp.path()).expect_err("schema mismatch");
        assert!(matches!(err, LoadError::SchemaVersion { .. }));
    }

    #[test]
    fn load_propagates_parse_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), PUBLISHERS_FILE, "schema_version = \"oops\"\n");
        let err = AuditRules::load_from(tmp.path()).expect_err("parse error");
        assert!(matches!(err, LoadError::Parse { .. }));
    }
}
