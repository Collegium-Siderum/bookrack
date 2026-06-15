// SPDX-License-Identifier: Apache-2.0

//! Audit profile for the papers pipeline.
//!
//! Loaded from a schema-locked default embedded at build time plus an
//! optional runtime overlay under `<data_root>/audit-rules/`. The
//! shape mirrors `bookrack-audit-profile` so operators learn one
//! overlay convention.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File name of the runtime overlay, looked up under
/// `<data_root>/audit-rules/`.
pub const PROFILE_OVERLAY_FILE: &str = "paper_audit_profile.local.toml";

/// Schema version the loader accepts. Bumped only when a renamed or
/// removed field changes its on-disk meaning.
pub const SCHEMA_VERSION: u32 = 1;

/// In-repo default profile source, embedded at build time.
pub const DEFAULT_PROFILE_TOML: &str = include_str!("../../data/paper_audit_profile.toml");

/// Built-in profile preset names.
pub const PROFILE_DEFAULT: &str = "default";
pub const PROFILE_TRUST_SOURCE: &str = "trust-source";
pub const PROFILE_STRICT: &str = "strict";

/// Names of every built-in profile preset that
/// [`PaperAuditProfile::from_named`] resolves.
pub const ALL_BUILT_IN_NAMES: &[&str] = &[PROFILE_DEFAULT, PROFILE_TRUST_SOURCE, PROFILE_STRICT];

/// Audit profile consumed by the papers pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperAuditProfile {
    /// Symbolic name stamped into `node_reviews.reviewed_by` as
    /// `bookrack-glean:<name>`.
    pub name: String,
    /// Master switch. When false the audit substep is skipped: the
    /// pipeline still writes a `pending` review row but produces no
    /// [`crate::audit::PaperReport`] and writes no audit-derived
    /// columns.
    pub audit_enabled: bool,
    pub identifier: IdentifierToggles,
    pub abstract_: AbstractToggles,
    pub author: AuthorToggles,
    pub title: TitleToggles,
    pub year: YearToggles,
    pub language: LanguageToggles,
    pub venue: VenueToggles,
    pub source_prior: SourcePriorToggles,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifierToggles {
    pub require_any: bool,
    pub doi_format_check: bool,
    pub arxiv_format_check: bool,
    pub issn_checksum_check: bool,
    pub orcid_checksum_check: bool,
}

impl Default for IdentifierToggles {
    fn default() -> Self {
        Self {
            require_any: true,
            doi_format_check: true,
            arxiv_format_check: true,
            issn_checksum_check: true,
            orcid_checksum_check: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbstractToggles {
    pub required: bool,
    pub min_chars: u32,
}

impl Default for AbstractToggles {
    fn default() -> Self {
        Self {
            required: true,
            min_chars: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorToggles {
    pub required: bool,
    pub sentinel_check: bool,
    pub single_word_check: bool,
}

impl Default for AuthorToggles {
    fn default() -> Self {
        Self {
            required: true,
            sentinel_check: true,
            single_word_check: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleToggles {
    pub required: bool,
    pub placeholder_check: bool,
    pub empty_check: bool,
    pub echoes_arxiv_banner_check: bool,
    pub equals_filename_check: bool,
}

impl Default for TitleToggles {
    fn default() -> Self {
        Self {
            required: true,
            placeholder_check: true,
            empty_check: true,
            echoes_arxiv_banner_check: true,
            equals_filename_check: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YearToggles {
    pub required: bool,
    pub range_check: bool,
    pub min: i32,
    pub max: i32,
    pub pdf_likely_file_date: bool,
    pub timestamp_form: bool,
}

impl Default for YearToggles {
    fn default() -> Self {
        Self {
            required: true,
            range_check: true,
            min: 1900,
            max: 2100,
            pdf_likely_file_date: true,
            timestamp_form: true,
        }
    }
}

/// Body-script ratios are stored as basis points (0..=10_000) so the
/// type carries `Eq`; helpers expose them as floats at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageToggles {
    pub bcp47_check: bool,
    pub body_script_match: bool,
    pub cjk_codes: Vec<String>,
    pub latin_codes: Vec<String>,
    body_cjk_min_ratio_bp: u32,
    body_latin_min_ratio_bp: u32,
    body_cjk_max_ratio_bp: u32,
}

impl LanguageToggles {
    /// Minimum CJK character ratio in `0.0..=1.0`.
    pub fn body_cjk_min_ratio(&self) -> f64 {
        f64::from(self.body_cjk_min_ratio_bp) / 10_000.0
    }
    /// Minimum Latin character ratio in `0.0..=1.0`.
    pub fn body_latin_min_ratio(&self) -> f64 {
        f64::from(self.body_latin_min_ratio_bp) / 10_000.0
    }
    /// Maximum CJK character ratio when a Latin language is declared,
    /// in `0.0..=1.0`.
    pub fn body_cjk_max_ratio(&self) -> f64 {
        f64::from(self.body_cjk_max_ratio_bp) / 10_000.0
    }
}

impl Default for LanguageToggles {
    fn default() -> Self {
        Self {
            bcp47_check: true,
            body_script_match: true,
            cjk_codes: vec!["zh".into(), "ja".into(), "ko".into()],
            latin_codes: vec![
                "en".into(),
                "fr".into(),
                "de".into(),
                "es".into(),
                "it".into(),
                "pt".into(),
                "nl".into(),
                "sv".into(),
                "no".into(),
                "da".into(),
                "fi".into(),
                "pl".into(),
                "ru".into(),
            ],
            body_cjk_min_ratio_bp: 1_000,
            body_latin_min_ratio_bp: 6_000,
            body_cjk_max_ratio_bp: 4_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueToggles {
    pub whitelist_check: bool,
}

impl Default for VenueToggles {
    fn default() -> Self {
        Self {
            whitelist_check: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePriorToggles {
    pub enabled: bool,
}

impl Default for SourcePriorToggles {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl PaperAuditProfile {
    /// The shipped `default` profile, parsed from
    /// [`DEFAULT_PROFILE_TOML`].
    pub fn default_profile() -> Self {
        parse_str(DEFAULT_PROFILE_TOML, PROFILE_DEFAULT)
            .expect("shipped default paper_audit_profile.toml must parse")
    }

    /// The `trust-source` profile: every toggle off, every named
    /// section disabled. `audit_enabled = false` short-circuits the
    /// audit substep at the call site.
    pub fn trust_source() -> Self {
        Self {
            name: PROFILE_TRUST_SOURCE.into(),
            audit_enabled: false,
            identifier: IdentifierToggles {
                require_any: false,
                doi_format_check: false,
                arxiv_format_check: false,
                issn_checksum_check: false,
                orcid_checksum_check: false,
            },
            abstract_: AbstractToggles {
                required: false,
                ..AbstractToggles::default()
            },
            author: AuthorToggles {
                required: false,
                sentinel_check: false,
                single_word_check: false,
            },
            title: TitleToggles {
                required: false,
                placeholder_check: false,
                empty_check: false,
                echoes_arxiv_banner_check: false,
                equals_filename_check: false,
            },
            year: YearToggles {
                required: false,
                range_check: false,
                pdf_likely_file_date: false,
                timestamp_form: false,
                ..YearToggles::default()
            },
            language: LanguageToggles {
                bcp47_check: false,
                body_script_match: false,
                ..LanguageToggles::default()
            },
            venue: VenueToggles {
                whitelist_check: false,
            },
            source_prior: SourcePriorToggles { enabled: false },
        }
    }

    /// The `strict` profile: built on `default`. Reserved for a
    /// future tightening of thresholds; today it is the default with
    /// a different name.
    pub fn strict() -> Self {
        Self {
            name: PROFILE_STRICT.into(),
            ..Self::default_profile()
        }
    }

    /// Resolve a named built-in profile. Returns `None` for any other
    /// string so the caller can fall back to the overlay path.
    pub fn from_named(name: &str) -> Option<Self> {
        match name {
            PROFILE_DEFAULT => Some(Self::default_profile()),
            PROFILE_TRUST_SOURCE => Some(Self::trust_source()),
            PROFILE_STRICT => Some(Self::strict()),
            _ => None,
        }
    }

    /// Load the profile from disk. The schema-locked default is
    /// parsed first; an optional overlay at
    /// `<dir>/paper_audit_profile.local.toml` is then merged on top.
    /// A missing directory or missing overlay yields the shipped
    /// default; a malformed overlay returns an error.
    pub fn load_from(dir: &Path) -> Result<Self, LoadError> {
        let mut profile = Self::default_profile();
        let overlay_path = dir.join(PROFILE_OVERLAY_FILE);
        if overlay_path.exists() {
            let raw = std::fs::read_to_string(&overlay_path).map_err(|error| LoadError::Io {
                path: overlay_path.clone(),
                error,
            })?;
            merge_overlay(&mut profile, &raw, &overlay_path)?;
        }
        Ok(profile)
    }
}

impl Default for PaperAuditProfile {
    fn default() -> Self {
        Self::default_profile()
    }
}

/// Reasons a load can fail.
#[derive(Debug)]
pub enum LoadError {
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

impl std::fmt::Display for LoadError {
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

impl std::error::Error for LoadError {}

fn parse_str(toml_str: &str, name: &str) -> Result<PaperAuditProfile, LoadError> {
    let synthetic = PathBuf::from(format!("<embedded:{name}>"));
    let file: ProfileFile = toml::from_str(toml_str).map_err(|error| LoadError::Parse {
        path: synthetic.clone(),
        error,
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(LoadError::SchemaVersion {
            path: synthetic,
            found: file.schema_version,
        });
    }
    let mut profile = PaperAuditProfile {
        name: name.to_string(),
        audit_enabled: true,
        identifier: IdentifierToggles::default(),
        abstract_: AbstractToggles::default(),
        author: AuthorToggles::default(),
        title: TitleToggles::default(),
        year: YearToggles::default(),
        language: LanguageToggles::default(),
        venue: VenueToggles::default(),
        source_prior: SourcePriorToggles::default(),
    };
    apply_overlay(&mut profile, file);
    Ok(profile)
}

fn merge_overlay(profile: &mut PaperAuditProfile, raw: &str, path: &Path) -> Result<(), LoadError> {
    let file: ProfileFile = toml::from_str(raw).map_err(|error| LoadError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(LoadError::SchemaVersion {
            path: path.to_path_buf(),
            found: file.schema_version,
        });
    }
    apply_overlay(profile, file);
    Ok(())
}

fn apply_overlay(profile: &mut PaperAuditProfile, file: ProfileFile) {
    if let Some(v) = file.audit_enabled {
        profile.audit_enabled = v;
    }
    if let Some(s) = file.identifier {
        if let Some(v) = s.require_any {
            profile.identifier.require_any = v;
        }
        if let Some(v) = s.doi_format_check {
            profile.identifier.doi_format_check = v;
        }
        if let Some(v) = s.arxiv_format_check {
            profile.identifier.arxiv_format_check = v;
        }
        if let Some(v) = s.issn_checksum_check {
            profile.identifier.issn_checksum_check = v;
        }
        if let Some(v) = s.orcid_checksum_check {
            profile.identifier.orcid_checksum_check = v;
        }
    }
    if let Some(s) = file.abstract_ {
        if let Some(v) = s.required {
            profile.abstract_.required = v;
        }
        if let Some(v) = s.min_chars {
            profile.abstract_.min_chars = v;
        }
    }
    if let Some(s) = file.author {
        if let Some(v) = s.required {
            profile.author.required = v;
        }
        if let Some(v) = s.sentinel_check {
            profile.author.sentinel_check = v;
        }
        if let Some(v) = s.single_word_check {
            profile.author.single_word_check = v;
        }
    }
    if let Some(s) = file.title {
        if let Some(v) = s.required {
            profile.title.required = v;
        }
        if let Some(v) = s.placeholder_check {
            profile.title.placeholder_check = v;
        }
        if let Some(v) = s.empty_check {
            profile.title.empty_check = v;
        }
        if let Some(v) = s.echoes_arxiv_banner_check {
            profile.title.echoes_arxiv_banner_check = v;
        }
        if let Some(v) = s.equals_filename_check {
            profile.title.equals_filename_check = v;
        }
    }
    if let Some(s) = file.year {
        if let Some(v) = s.required {
            profile.year.required = v;
        }
        if let Some(v) = s.range_check {
            profile.year.range_check = v;
        }
        if let Some(v) = s.min {
            profile.year.min = v;
        }
        if let Some(v) = s.max {
            profile.year.max = v;
        }
        if let Some(v) = s.pdf_likely_file_date {
            profile.year.pdf_likely_file_date = v;
        }
        if let Some(v) = s.timestamp_form {
            profile.year.timestamp_form = v;
        }
    }
    if let Some(s) = file.language {
        if let Some(v) = s.bcp47_check {
            profile.language.bcp47_check = v;
        }
        if let Some(v) = s.body_script_match {
            profile.language.body_script_match = v;
        }
        if let Some(v) = s.cjk_codes {
            profile.language.cjk_codes = v;
        }
        if let Some(v) = s.latin_codes {
            profile.language.latin_codes = v;
        }
        if let Some(v) = s.body_cjk_min_ratio {
            profile.language.body_cjk_min_ratio_bp = (v * 10_000.0) as u32;
        }
        if let Some(v) = s.body_latin_min_ratio {
            profile.language.body_latin_min_ratio_bp = (v * 10_000.0) as u32;
        }
        if let Some(v) = s.body_cjk_max_ratio {
            profile.language.body_cjk_max_ratio_bp = (v * 10_000.0) as u32;
        }
    }
    if let Some(s) = file.venue
        && let Some(v) = s.whitelist_check
    {
        profile.venue.whitelist_check = v;
    }
    if let Some(s) = file.source_prior
        && let Some(v) = s.enabled
    {
        profile.source_prior.enabled = v;
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    schema_version: u32,
    #[serde(default)]
    audit_enabled: Option<bool>,
    #[serde(default)]
    identifier: Option<IdentifierSection>,
    #[serde(default, rename = "abstract")]
    abstract_: Option<AbstractSection>,
    #[serde(default)]
    author: Option<AuthorSection>,
    #[serde(default)]
    title: Option<TitleSection>,
    #[serde(default)]
    year: Option<YearSection>,
    #[serde(default)]
    language: Option<LanguageSection>,
    #[serde(default)]
    venue: Option<VenueSection>,
    #[serde(default)]
    source_prior: Option<SourcePriorSection>,
}

macro_rules! optional_section {
    ($name:ident { $( $field:ident: $ty:ty ),* $(,)? }) => {
        #[derive(Debug, Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct $name {
            $( #[serde(default)] $field: Option<$ty> ),*
        }
    };
}

optional_section!(IdentifierSection {
    require_any: bool,
    doi_format_check: bool,
    arxiv_format_check: bool,
    issn_checksum_check: bool,
    orcid_checksum_check: bool,
});

optional_section!(AbstractSection {
    required: bool,
    min_chars: u32,
});

optional_section!(AuthorSection {
    required: bool,
    sentinel_check: bool,
    single_word_check: bool,
});

optional_section!(TitleSection {
    required: bool,
    placeholder_check: bool,
    empty_check: bool,
    echoes_arxiv_banner_check: bool,
    equals_filename_check: bool,
});

optional_section!(YearSection {
    required: bool,
    range_check: bool,
    min: i32,
    max: i32,
    pdf_likely_file_date: bool,
    timestamp_form: bool,
});

optional_section!(LanguageSection {
    bcp47_check: bool,
    body_script_match: bool,
    cjk_codes: Vec<String>,
    latin_codes: Vec<String>,
    body_cjk_min_ratio: f64,
    body_latin_min_ratio: f64,
    body_cjk_max_ratio: f64,
});

optional_section!(VenueSection {
    whitelist_check: bool
});

optional_section!(SourcePriorSection { enabled: bool });

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_profile_parses_and_carries_the_default_name() {
        let p = PaperAuditProfile::default_profile();
        assert_eq!(p.name, PROFILE_DEFAULT);
        assert!(p.audit_enabled);
        assert_eq!(p.abstract_.min_chars, 200);
        assert_eq!(p.year.min, 1900);
        assert_eq!(p.year.max, 2100);
        assert!(p.identifier.require_any);
    }

    #[test]
    fn trust_source_disables_every_toggle() {
        let p = PaperAuditProfile::trust_source();
        assert_eq!(p.name, PROFILE_TRUST_SOURCE);
        assert!(!p.audit_enabled);
        assert!(!p.identifier.require_any);
        assert!(!p.title.required);
        assert!(!p.year.required);
        assert!(!p.venue.whitelist_check);
        assert!(!p.source_prior.enabled);
    }

    #[test]
    fn strict_carries_its_own_name() {
        let p = PaperAuditProfile::strict();
        assert_eq!(p.name, PROFILE_STRICT);
        assert!(p.audit_enabled);
    }

    #[test]
    fn from_named_resolves_three_presets_and_returns_none_for_unknown() {
        assert_eq!(
            PaperAuditProfile::from_named(PROFILE_DEFAULT).map(|p| p.name),
            Some(PROFILE_DEFAULT.to_string())
        );
        assert_eq!(
            PaperAuditProfile::from_named(PROFILE_TRUST_SOURCE).map(|p| p.name),
            Some(PROFILE_TRUST_SOURCE.to_string())
        );
        assert_eq!(
            PaperAuditProfile::from_named(PROFILE_STRICT).map(|p| p.name),
            Some(PROFILE_STRICT.to_string())
        );
        assert!(PaperAuditProfile::from_named("not-a-preset").is_none());
    }

    #[test]
    fn load_from_missing_dir_returns_default_profile() {
        let dir = TempDir::new().unwrap();
        let p = PaperAuditProfile::load_from(dir.path()).unwrap();
        assert_eq!(p, PaperAuditProfile::default_profile());
    }

    #[test]
    fn overlay_with_partial_field_set_merges_field_by_field() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(PROFILE_OVERLAY_FILE);
        std::fs::write(
            &overlay,
            "schema_version = 1\n\
             [abstract]\n\
             min_chars = 500\n\
             [identifier]\n\
             require_any = false\n",
        )
        .unwrap();
        let p = PaperAuditProfile::load_from(dir.path()).unwrap();
        assert_eq!(p.abstract_.min_chars, 500);
        assert!(!p.identifier.require_any);
        // unchanged fields keep their default values
        assert!(p.identifier.doi_format_check);
        assert!(p.title.required);
    }

    #[test]
    fn overlay_with_wrong_schema_version_fails() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(PROFILE_OVERLAY_FILE);
        std::fs::write(&overlay, "schema_version = 99\n").unwrap();
        let err = PaperAuditProfile::load_from(dir.path()).unwrap_err();
        assert!(matches!(err, LoadError::SchemaVersion { .. }));
    }

    #[test]
    fn overlay_with_unknown_field_fails() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(PROFILE_OVERLAY_FILE);
        std::fs::write(&overlay, "schema_version = 1\nnonsense_top_level = true\n").unwrap();
        let err = PaperAuditProfile::load_from(dir.path()).unwrap_err();
        assert!(matches!(err, LoadError::Parse { .. }));
    }
}
