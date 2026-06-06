// SPDX-License-Identifier: Apache-2.0

//! Multi-language heading patterns the TXT adapter consults to
//! recognise chapter / volume marker lines.
//!
//! Three template families cover the long-form-prose corpus the
//! adapter sees today:
//!
//! - **Sino** — `<prefix><numeral><unit>` as one uninterrupted word.
//!   Covers Simplified and Traditional Chinese, Japanese, and
//!   Sino-Korean conventions that route through the same shape.
//! - **Latin** — `<UnitWord> <Numeral>[ rest]` with the unit word
//!   followed by Roman, Arabic, or a small set of spelled-out first
//!   ordinals. Covers English, French, Spanish, Italian.
//! - **German** — `<SpelledOrdinalAdj> <Unit>` as exactly two
//!   tokens. Covers German novels whose chapter labels are always
//!   spelled out.
//!
//! Two sources feed [`HeadingPatterns::load_from`]:
//!
//! 1. The crate ships a schema-locked default at
//!    `crates/audit-profile/data/headings.toml`, embedded through
//!    `include_str!`.
//! 2. An optional overlay at `<data_root>/audit-rules/headings.toml`.
//!    Each field present REPLACES the shipped default; missing
//!    fields fall through. A malformed file aborts the load.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File name of the runtime overlay, looked up under
/// `<data_root>/audit-rules/`.
pub const HEADINGS_OVERLAY_FILE: &str = "headings.toml";
/// Schema version the loader accepts.
pub const HEADINGS_SCHEMA_VERSION: u32 = 1;

/// In-repo default heading patterns, embedded at build time.
pub const DEFAULT_HEADINGS_TOML: &str = include_str!("../data/headings.toml");

/// Patterns the TXT adapter's heading dispatcher consults, one
/// sub-struct per template family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingPatterns {
    pub sino: SinoPatterns,
    pub latin: LatinPatterns,
    pub german: GermanPatterns,
}

/// Sino template: `<prefix><numeral><unit>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinoPatterns {
    /// The ordinal prefix character that opens the marker. Stored as
    /// a string for TOML ergonomics; only the first scalar is used.
    pub prefix: String,
    /// Volume-class unit single-character strings. Level-1 heading.
    pub volume_units: Vec<String>,
    /// Chapter-class unit single-character strings. Level-2 heading.
    pub chapter_units: Vec<String>,
    /// CJK numerals plus the fullwidth and formal digit sets, packed
    /// into one string; membership is tested by `contains(char)`.
    pub numerals: String,
    /// Character cap for one matching heading line.
    pub max_chars: usize,
}

/// Latin template: `<UnitWord> <Numeral>[ rest]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatinPatterns {
    pub volume_units: Vec<String>,
    pub chapter_units: Vec<String>,
    /// Spelled-out first-ordinal words observed in the wild.
    pub spelled_first: Vec<String>,
    /// Roman-numeral alphabet packed into one string.
    pub roman_chars: String,
    /// Maximum length of a permissive Roman-numeral run.
    pub roman_max_len: usize,
    /// Character cap for one matching heading line.
    pub max_chars: usize,
}

/// German template: `<SpelledOrdinalAdj> <Unit>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GermanPatterns {
    /// Ordinal stems 1 through 20, plus the alternate form of 7.
    pub ordinal_stems: Vec<String>,
    /// Character cap for one matching heading line.
    pub max_chars: usize,
}

impl HeadingPatterns {
    /// The shipped default patterns: reproduce the pre-externalisation
    /// hard-coded behaviour field-for-field.
    pub fn default_patterns() -> Self {
        parse_str(DEFAULT_HEADINGS_TOML, &PathBuf::from("<embedded:headings>"))
            .expect("shipped default headings.toml must parse")
    }

    /// Load patterns from disk. The schema-locked default is parsed
    /// first; an optional overlay at `<dir>/headings.toml` is then
    /// merged on top. A missing directory or missing overlay yields
    /// the shipped default; a malformed overlay returns an error.
    pub fn load_from(dir: &Path) -> Result<Self, HeadingsLoadError> {
        if !dir.exists() {
            return Ok(Self::default_patterns());
        }
        let mut patterns = Self::default_patterns();
        let overlay_path = dir.join(HEADINGS_OVERLAY_FILE);
        if overlay_path.exists() {
            let raw =
                std::fs::read_to_string(&overlay_path).map_err(|error| HeadingsLoadError::Io {
                    path: overlay_path.clone(),
                    error,
                })?;
            merge_overlay(&mut patterns, &raw, &overlay_path)?;
        }
        Ok(patterns)
    }
}

impl Default for HeadingPatterns {
    fn default() -> Self {
        Self::default_patterns()
    }
}

/// Reasons a heading-patterns load can fail.
#[derive(Debug)]
pub enum HeadingsLoadError {
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

impl std::fmt::Display for HeadingsLoadError {
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
                "unsupported schema_version {found} in {} (expected {HEADINGS_SCHEMA_VERSION})",
                path.display()
            ),
        }
    }
}

impl std::error::Error for HeadingsLoadError {}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HeadingsFile {
    schema_version: u32,
    #[serde(default)]
    sino: Option<SinoSection>,
    #[serde(default)]
    latin: Option<LatinSection>,
    #[serde(default)]
    german: Option<GermanSection>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SinoSection {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    volume_units: Option<Vec<String>>,
    #[serde(default)]
    chapter_units: Option<Vec<String>>,
    #[serde(default)]
    numerals: Option<String>,
    #[serde(default)]
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LatinSection {
    #[serde(default)]
    volume_units: Option<Vec<String>>,
    #[serde(default)]
    chapter_units: Option<Vec<String>>,
    #[serde(default)]
    spelled_first: Option<Vec<String>>,
    #[serde(default)]
    roman_chars: Option<String>,
    #[serde(default)]
    roman_max_len: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GermanSection {
    #[serde(default)]
    ordinal_stems: Option<Vec<String>>,
    #[serde(default)]
    max_chars: Option<usize>,
}

fn parse_str(raw: &str, path: &Path) -> Result<HeadingPatterns, HeadingsLoadError> {
    let file: HeadingsFile = toml::from_str(raw).map_err(|error| HeadingsLoadError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    if file.schema_version != HEADINGS_SCHEMA_VERSION {
        return Err(HeadingsLoadError::SchemaVersion {
            path: path.to_path_buf(),
            found: file.schema_version,
        });
    }
    let mut patterns = blank_patterns();
    apply_overlay(&mut patterns, file);
    Ok(patterns)
}

fn merge_overlay(
    patterns: &mut HeadingPatterns,
    raw: &str,
    path: &Path,
) -> Result<(), HeadingsLoadError> {
    let file: HeadingsFile = toml::from_str(raw).map_err(|error| HeadingsLoadError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    if file.schema_version != HEADINGS_SCHEMA_VERSION {
        return Err(HeadingsLoadError::SchemaVersion {
            path: path.to_path_buf(),
            found: file.schema_version,
        });
    }
    apply_overlay(patterns, file);
    Ok(())
}

fn blank_patterns() -> HeadingPatterns {
    HeadingPatterns {
        sino: SinoPatterns {
            prefix: String::new(),
            volume_units: Vec::new(),
            chapter_units: Vec::new(),
            numerals: String::new(),
            max_chars: 0,
        },
        latin: LatinPatterns {
            volume_units: Vec::new(),
            chapter_units: Vec::new(),
            spelled_first: Vec::new(),
            roman_chars: String::new(),
            roman_max_len: 0,
            max_chars: 0,
        },
        german: GermanPatterns {
            ordinal_stems: Vec::new(),
            max_chars: 0,
        },
    }
}

fn apply_overlay(patterns: &mut HeadingPatterns, file: HeadingsFile) {
    if let Some(s) = file.sino {
        if let Some(v) = s.prefix {
            patterns.sino.prefix = v;
        }
        if let Some(v) = s.volume_units {
            patterns.sino.volume_units = v;
        }
        if let Some(v) = s.chapter_units {
            patterns.sino.chapter_units = v;
        }
        if let Some(v) = s.numerals {
            patterns.sino.numerals = v;
        }
        if let Some(v) = s.max_chars {
            patterns.sino.max_chars = v;
        }
    }
    if let Some(s) = file.latin {
        if let Some(v) = s.volume_units {
            patterns.latin.volume_units = v;
        }
        if let Some(v) = s.chapter_units {
            patterns.latin.chapter_units = v;
        }
        if let Some(v) = s.spelled_first {
            patterns.latin.spelled_first = v;
        }
        if let Some(v) = s.roman_chars {
            patterns.latin.roman_chars = v;
        }
        if let Some(v) = s.roman_max_len {
            patterns.latin.roman_max_len = v;
        }
        if let Some(v) = s.max_chars {
            patterns.latin.max_chars = v;
        }
    }
    if let Some(s) = file.german {
        if let Some(v) = s.ordinal_stems {
            patterns.german.ordinal_stems = v;
        }
        if let Some(v) = s.max_chars {
            patterns.german.max_chars = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_patterns_reproduce_hard_coded_behaviour() {
        let p = HeadingPatterns::default_patterns();
        // Sino: the ordinal prefix is U+7B2C.
        assert_eq!(p.sino.prefix.chars().next(), Some('\u{7B2C}'));
        assert_eq!(p.sino.volume_units.len(), 4);
        assert_eq!(p.sino.chapter_units.len(), 5);
        assert!(p.sino.numerals.contains('\u{4E00}'));
        assert!(p.sino.numerals.contains('\u{FF19}'));
        assert_eq!(p.sino.max_chars, 60);
        // Latin: cap and unit word counts match the previous source consts.
        assert_eq!(p.latin.volume_units.len(), 6);
        assert_eq!(p.latin.chapter_units.len(), 4);
        assert_eq!(p.latin.roman_chars, "IVXLCDM");
        assert_eq!(p.latin.roman_max_len, 8);
        assert_eq!(p.latin.max_chars, 100);
        assert!(p.latin.spelled_first.iter().any(|s| s == "first"));
        // German: 20 ordinal stems plus the siebent alternate.
        assert_eq!(p.german.ordinal_stems.len(), 21);
        assert_eq!(p.german.max_chars, 30);
    }

    #[test]
    fn load_from_empty_directory_yields_default_patterns() {
        let dir = TempDir::new().unwrap();
        let loaded = HeadingPatterns::load_from(dir.path()).unwrap();
        assert_eq!(loaded, HeadingPatterns::default_patterns());
    }

    #[test]
    fn overlay_replaces_named_fields_only() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(HEADINGS_OVERLAY_FILE),
            "schema_version = 1\n[german]\nmax_chars = 60\n",
        )
        .unwrap();
        let loaded = HeadingPatterns::load_from(dir.path()).unwrap();
        assert_eq!(loaded.german.max_chars, 60);
        // Untouched fields keep the shipped default.
        assert_eq!(loaded.german.ordinal_stems.len(), 21);
        assert_eq!(loaded.latin.max_chars, 100);
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(HEADINGS_OVERLAY_FILE),
            "schema_version = 99\n",
        )
        .unwrap();
        let err = HeadingPatterns::load_from(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            HeadingsLoadError::SchemaVersion { found: 99, .. }
        ));
    }
}
