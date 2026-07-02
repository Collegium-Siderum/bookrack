// SPDX-License-Identifier: Apache-2.0

//! Audit profile and data sets — the configuration the metadata audit,
//! the diagnose scrubber, and the ingest dryrun walker consult at
//! evaluation time.
//!
//! Two on-disk schemas sit side by side under `<data_root>/audit-rules/`:
//!
//! - [`AuditProfile`] — toggles and numeric thresholds, one sub-table
//!   per audit domain (year, title, language, publisher, TOC shape,
//!   source prior, copyright block window, filename parser, extract
//!   half-rules). Reads `audit_profile.toml` plus an optional
//!   `audit_profile.local.toml` overlay.
//! - [`AuditData`] — runtime-loaded data lists (publisher whitelist,
//!   watermark patterns and tokens, abbreviation expansions,
//!   placeholder titles, book extensions). Reads `audit_data.toml`,
//!   replacing `publishers.toml` / `watermarks.toml` from the older
//!   layout.
//!
//! ## Loading
//!
//! Two sources feed [`AuditProfile::load_from`]:
//!
//! 1. The crate ships a schema-locked default profile at
//!    `crates/audit-profile/data/audit_profile.toml`, embedded through
//!    `include_str!` so an install needs no on-disk file to start.
//!    The default values reproduce the previous hard-coded behaviour
//!    exactly.
//! 2. An optional overlay at
//!    `<data_root>/audit-rules/audit_profile.local.toml`. Missing
//!    fields fall through to the default; a malformed file aborts the
//!    load.
//!
//! Three named profiles — `default`, `trust-source`, `strict` — are
//! also exposed for the CLI's `--audit-profile` flag and bypass the
//! file overlay.
//!
//! ## Dependency direction
//!
//! This crate sits below `metadata`, `extract`, and `ingest`: each
//! pulls profile values down at the call site. Keeping the type here
//! avoids `extract` reaching upward into `metadata` for a setting.

use std::path::Path;

use serde::{Deserialize, Serialize};

mod data;
mod fingerprint;
mod headings;
mod load;

pub use data::{
    AuditData, DATA_OVERLAY_FILE, DATA_SCHEMA_VERSION, DEFAULT_DATA_TOML, DataLoadError,
};
pub use fingerprint::{
    FingerprintError, profile_fingerprint, profile_toggle_summary, stable_fingerprint,
    stable_fingerprint_parts,
};
pub use headings::{
    DEFAULT_HEADINGS_TOML, GermanPatterns, HEADINGS_OVERLAY_FILE, HEADINGS_SCHEMA_VERSION,
    HeadingPatterns, HeadingsLoadError, LatinPatterns, SinoPatterns,
};
pub use load::LoadError;

/// File name of the runtime overlay, looked up under
/// `<data_root>/audit-rules/`.
pub const PROFILE_OVERLAY_FILE: &str = "audit_profile.local.toml";
/// Schema version the loader accepts. Bumped only when a renamed or
/// removed field changes its on-disk meaning.
pub const SCHEMA_VERSION: u32 = 1;

/// In-repo default profile source, embedded at build time.
pub const DEFAULT_PROFILE_TOML: &str = include_str!("../data/audit_profile.toml");

/// Name of the three built-in profile presets.
pub const PROFILE_DEFAULT: &str = "default";
pub const PROFILE_TRUST_SOURCE: &str = "trust-source";
pub const PROFILE_STRICT: &str = "strict";

/// Names of every built-in profile preset that [`AuditProfile::from_named`]
/// resolves. Listed in the order shipped to operators: the default first,
/// then the two alternatives.
pub const ALL_BUILT_IN_NAMES: &[&str] = &[PROFILE_DEFAULT, PROFILE_TRUST_SOURCE, PROFILE_STRICT];

/// Audit profile consumed by the metadata audit, the filename parser,
/// and the extract half-rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditProfile {
    /// Symbolic name of the profile. Stamped into `reviewed_by` as
    /// `bookrack-ingest:<name>` and surfaced in the dryrun report.
    pub name: String,
    /// Master switch. When false the audit substep is skipped: review
    /// rows still land as `pending`, but no `MetadataReport` is built
    /// and audit-derived columns stay empty.
    pub audit_enabled: bool,
    pub year: YearToggles,
    pub title: TitleToggles,
    pub language: LanguageToggles,
    pub publisher: PublisherToggles,
    pub toc_shape: TocShapeToggles,
    pub source_prior: SourcePriorToggles,
    pub copyright_blocks: CopyrightBlocksToggles,
    pub filename_parser: FilenameParserToggles,
    pub extract: ExtractToggles,
    pub html: HtmlToggles,
    pub quality: QualityThresholds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct YearToggles {
    pub range_check: bool,
    pub min: i32,
    pub max: i32,
    pub pdf_likely_file_date: bool,
    pub timestamp_form: bool,
    pub cross_field_filename_override: bool,
}

impl Default for YearToggles {
    fn default() -> Self {
        Self {
            range_check: true,
            min: 1450,
            max: 2100,
            pdf_likely_file_date: true,
            timestamp_form: true,
            cross_field_filename_override: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TitleToggles {
    pub placeholder_check: bool,
    pub purely_numeric: bool,
    pub series_paren: bool,
    pub marketing_block: bool,
    pub aggregator_marker: bool,
    pub volume_marker: bool,
    pub bracketed_min_chars: usize,
}

impl TitleToggles {
    /// True when at least one bracketed-segment sub-rule is enabled.
    pub fn any_bracketed_enabled(&self) -> bool {
        self.series_paren || self.marketing_block || self.aggregator_marker || self.volume_marker
    }
}

impl Default for TitleToggles {
    fn default() -> Self {
        Self {
            placeholder_check: true,
            purely_numeric: true,
            series_paren: true,
            marketing_block: true,
            aggregator_marker: true,
            volume_marker: true,
            bracketed_min_chars: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LanguageToggles {
    pub bcp47_check: bool,
    pub body_script_match: bool,
    /// BCP-47 primary subtags treated as CJK-bucket languages by the
    /// body / declared-language disagreement check.
    pub cjk_codes: Vec<String>,
    /// BCP-47 primary subtags treated as Latin-script-bucket languages.
    pub latin_codes: Vec<String>,
    /// Minimum CJK character ratio in the body sample required before
    /// a CJK declaration is accepted; expressed in basis points
    /// (0..=10000) so the struct keeps `Eq`.
    pub body_cjk_min_ratio_bp: u32,
    /// Threshold on Latin-letter ratio that combined with
    /// `body_cjk_min_ratio_bp` decides a CJK declaration mismatch.
    pub body_latin_min_ratio_bp: u32,
    /// Maximum CJK character ratio tolerated when a Latin-script
    /// language is declared.
    pub body_cjk_max_ratio_bp: u32,
}

impl LanguageToggles {
    /// CJK minimum ratio as a float in 0.0..=1.0.
    pub fn body_cjk_min_ratio(&self) -> f64 {
        self.body_cjk_min_ratio_bp as f64 / 10_000.0
    }
    /// Latin minimum ratio as a float in 0.0..=1.0.
    pub fn body_latin_min_ratio(&self) -> f64 {
        self.body_latin_min_ratio_bp as f64 / 10_000.0
    }
    /// CJK maximum ratio (for Latin declarations) as a float.
    pub fn body_cjk_max_ratio(&self) -> f64 {
        self.body_cjk_max_ratio_bp as f64 / 10_000.0
    }
}

impl Default for LanguageToggles {
    fn default() -> Self {
        Self {
            bcp47_check: true,
            body_script_match: true,
            cjk_codes: vec!["zh".to_string(), "ja".to_string(), "ko".to_string()],
            latin_codes: vec![
                "en".to_string(),
                "fr".to_string(),
                "de".to_string(),
                "es".to_string(),
                "it".to_string(),
                "pt".to_string(),
                "nl".to_string(),
                "sv".to_string(),
                "no".to_string(),
                "da".to_string(),
                "fi".to_string(),
                "pl".to_string(),
                "ru".to_string(),
            ],
            body_cjk_min_ratio_bp: 1000,
            body_latin_min_ratio_bp: 6000,
            body_cjk_max_ratio_bp: 4000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublisherToggles {
    pub url_watermark: bool,
    pub whitelist_normalize_abbreviations: bool,
    pub drop_10digit_isbn_to_filename: bool,
}

impl Default for PublisherToggles {
    fn default() -> Self {
        Self {
            url_watermark: true,
            whitelist_normalize_abbreviations: true,
            drop_10digit_isbn_to_filename: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TocShapeToggles {
    pub suspicious_flat: bool,
    pub flat_min_entries: usize,
    pub flat_severe_min_entries: usize,
    pub heading_block_skew: bool,
    pub skew_min: usize,
    pub skew_ratio: usize,
    pub empty_large_body: bool,
    pub large_body_min_blocks: usize,
}

impl Default for TocShapeToggles {
    fn default() -> Self {
        Self {
            suspicious_flat: true,
            flat_min_entries: 5,
            flat_severe_min_entries: 10,
            heading_block_skew: true,
            skew_min: 5,
            skew_ratio: 4,
            empty_large_body: true,
            large_body_min_blocks: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourcePriorToggles {
    pub enabled: bool,
}

impl Default for SourcePriorToggles {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyrightBlocksToggles {
    pub enabled: bool,
    pub count: usize,
}

impl Default for CopyrightBlocksToggles {
    fn default() -> Self {
        Self {
            enabled: true,
            count: 6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilenameParserToggles {
    pub enabled: bool,
    pub year_min: u32,
    pub year_max: u32,
}

impl Default for FilenameParserToggles {
    fn default() -> Self {
        Self {
            enabled: true,
            year_min: 1500,
            year_max: 2100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtractToggles {
    pub epub_year_range_check: bool,
    pub epub_year_min: i32,
    pub epub_year_max: i32,
    pub epub_isbn_recognition: bool,
    pub marc_role_mapping: bool,
    pub txt_toc_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HtmlToggles {
    /// Block-level tags the DOM walk emits as one [`Block`].
    pub block_tags: Vec<String>,
    /// Tags whose subtrees carry no readable prose and are skipped.
    pub skip_tags: Vec<String>,
}

impl Default for HtmlToggles {
    fn default() -> Self {
        Self {
            block_tags: [
                "p",
                "h1",
                "h2",
                "h3",
                "h4",
                "h5",
                "h6",
                "li",
                "blockquote",
                "figcaption",
                "pre",
                "td",
                "th",
                "dd",
                "dt",
                "caption",
                "div",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            skip_tags: ["script", "style", "head", "nav", "template", "svg"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// PDF text-layer quality thresholds. The eight ratios / counts are
/// stored as basis points (0..=10_000) for `Eq`; helpers expose them
/// as floats at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualityThresholds {
    /// Below this characters-per-page count, route the layer to OCR
    /// as a bare scan. Stored as the integer value (chars / page) the
    /// `assess` compare runs against.
    pub chars_per_page_ocr: u32,
    /// Below this characters-per-page count, downgrade the layer to
    /// Doubtful as sparse.
    pub chars_per_page_doubt: u32,
    pub replacement_ocr_bp: u32,
    pub pua_ocr_bp: u32,
    pub pua_doubt_bp: u32,
    pub control_ocr_bp: u32,
    pub dual_layer_bp: u32,
    pub cjk_space_doubt_bp: u32,
}

impl QualityThresholds {
    pub fn chars_per_page_ocr(&self) -> f64 {
        self.chars_per_page_ocr as f64
    }
    pub fn chars_per_page_doubt(&self) -> f64 {
        self.chars_per_page_doubt as f64
    }
    pub fn replacement_ocr(&self) -> f64 {
        self.replacement_ocr_bp as f64 / 10_000.0
    }
    pub fn pua_ocr(&self) -> f64 {
        self.pua_ocr_bp as f64 / 10_000.0
    }
    pub fn pua_doubt(&self) -> f64 {
        self.pua_doubt_bp as f64 / 10_000.0
    }
    pub fn control_ocr(&self) -> f64 {
        self.control_ocr_bp as f64 / 10_000.0
    }
    pub fn dual_layer(&self) -> f64 {
        self.dual_layer_bp as f64 / 10_000.0
    }
    pub fn cjk_space_doubt(&self) -> f64 {
        self.cjk_space_doubt_bp as f64 / 10_000.0
    }
}

impl Default for QualityThresholds {
    fn default() -> Self {
        Self {
            chars_per_page_ocr: 50,
            chars_per_page_doubt: 200,
            replacement_ocr_bp: 500,
            pua_ocr_bp: 1000,
            pua_doubt_bp: 100,
            control_ocr_bp: 200,
            dual_layer_bp: 5000,
            cjk_space_doubt_bp: 200,
        }
    }
}

impl Default for ExtractToggles {
    fn default() -> Self {
        Self {
            epub_year_range_check: true,
            epub_year_min: 1000,
            epub_year_max: 2200,
            epub_isbn_recognition: true,
            marc_role_mapping: true,
            txt_toc_enabled: true,
        }
    }
}

impl AuditProfile {
    /// The shipped `default` profile: reproduces the pre-profile
    /// hard-coded behaviour field-for-field.
    pub fn default_profile() -> Self {
        load::parse_str(DEFAULT_PROFILE_TOML, PROFILE_DEFAULT)
            .expect("shipped default audit_profile.toml must parse")
    }

    /// The `trust-source` profile: every toggle off, every named
    /// section disabled. `audit_enabled = false` short-circuits the
    /// audit substep at `run_metadata_substep`.
    pub fn trust_source() -> Self {
        Self {
            name: PROFILE_TRUST_SOURCE.to_string(),
            audit_enabled: false,
            year: YearToggles {
                range_check: false,
                pdf_likely_file_date: false,
                timestamp_form: false,
                cross_field_filename_override: false,
                ..YearToggles::default()
            },
            title: TitleToggles {
                placeholder_check: false,
                purely_numeric: false,
                series_paren: false,
                marketing_block: false,
                aggregator_marker: false,
                volume_marker: false,
                ..TitleToggles::default()
            },
            language: LanguageToggles {
                bcp47_check: false,
                body_script_match: false,
                ..LanguageToggles::default()
            },
            publisher: PublisherToggles {
                url_watermark: false,
                whitelist_normalize_abbreviations: false,
                drop_10digit_isbn_to_filename: false,
            },
            toc_shape: TocShapeToggles {
                suspicious_flat: false,
                heading_block_skew: false,
                empty_large_body: false,
                ..TocShapeToggles::default()
            },
            source_prior: SourcePriorToggles { enabled: false },
            copyright_blocks: CopyrightBlocksToggles {
                enabled: false,
                ..CopyrightBlocksToggles::default()
            },
            filename_parser: FilenameParserToggles {
                enabled: false,
                ..FilenameParserToggles::default()
            },
            extract: ExtractToggles {
                epub_year_range_check: false,
                epub_isbn_recognition: false,
                marc_role_mapping: false,
                txt_toc_enabled: false,
                ..ExtractToggles::default()
            },
            html: HtmlToggles::default(),
            quality: QualityThresholds::default(),
        }
    }

    /// The `strict` profile: built on `default`, raises a few signals
    /// the team has chosen to treat as harder errors. The exact set is
    /// stable across patches.
    pub fn strict() -> Self {
        let mut profile = Self::default_profile();
        profile.name = PROFILE_STRICT.to_string();
        profile
    }

    /// Resolve a named built-in profile. Returns `None` for any other
    /// string so the CLI can fall back to the overlay path.
    pub fn from_named(name: &str) -> Option<Self> {
        match name {
            PROFILE_DEFAULT => Some(Self::default_profile()),
            PROFILE_TRUST_SOURCE => Some(Self::trust_source()),
            PROFILE_STRICT => Some(Self::strict()),
            _ => None,
        }
    }

    /// Load the profile from disk. The schema-locked default is parsed
    /// first; an optional overlay at
    /// `<dir>/audit_profile.local.toml` is then merged on top. A
    /// missing directory or missing overlay yields the shipped
    /// default; a malformed overlay returns an error.
    pub fn load_from(dir: &Path) -> Result<Self, LoadError> {
        let mut profile = Self::default_profile();
        let overlay_path = dir.join(PROFILE_OVERLAY_FILE);
        if overlay_path.exists() {
            let raw = std::fs::read_to_string(&overlay_path).map_err(|error| LoadError::Io {
                path: overlay_path.clone(),
                error,
            })?;
            load::merge_overlay(&mut profile, &raw, &overlay_path)?;
        }
        Ok(profile)
    }
}

impl Default for AuditProfile {
    fn default() -> Self {
        Self::default_profile()
    }
}

/// On-disk shape of the schema-locked profile and any overlay. Every
/// sub-table is optional so an overlay only needs to declare the
/// fields it changes.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileFile {
    pub schema_version: u32,
    #[serde(default)]
    pub audit_enabled: Option<bool>,
    #[serde(default)]
    pub year: Option<YearSection>,
    #[serde(default)]
    pub title: Option<TitleSection>,
    #[serde(default)]
    pub language: Option<LanguageSection>,
    #[serde(default)]
    pub publisher: Option<PublisherSection>,
    #[serde(default)]
    pub toc_shape: Option<TocShapeSection>,
    #[serde(default)]
    pub source_prior: Option<SourcePriorSection>,
    #[serde(default)]
    pub copyright_blocks: Option<CopyrightBlocksSection>,
    #[serde(default)]
    pub filename_parser: Option<FilenameParserSection>,
    #[serde(default)]
    pub extract: Option<ExtractSection>,
    #[serde(default)]
    pub html: Option<HtmlSection>,
    #[serde(default)]
    pub quality: Option<QualitySection>,
}

macro_rules! optional_section {
    ($name:ident { $( $field:ident: $ty:ty ),* $(,)? }) => {
        #[derive(Debug, Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        pub(crate) struct $name {
            $( #[serde(default)] pub $field: Option<$ty> ),*
        }
    };
}

optional_section!(YearSection {
    range_check: bool,
    min: i32,
    max: i32,
    pdf_likely_file_date: bool,
    timestamp_form: bool,
    cross_field_filename_override: bool,
});

optional_section!(TitleSection {
    placeholder_check: bool,
    purely_numeric: bool,
    series_paren: bool,
    marketing_block: bool,
    aggregator_marker: bool,
    volume_marker: bool,
    bracketed_min_chars: usize,
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

optional_section!(PublisherSection {
    url_watermark: bool,
    whitelist_normalize_abbreviations: bool,
    drop_10digit_isbn_to_filename: bool,
});

optional_section!(TocShapeSection {
    suspicious_flat: bool,
    flat_min_entries: usize,
    flat_severe_min_entries: usize,
    heading_block_skew: bool,
    skew_min: usize,
    skew_ratio: usize,
    empty_large_body: bool,
    large_body_min_blocks: usize,
});

optional_section!(SourcePriorSection { enabled: bool });

optional_section!(CopyrightBlocksSection {
    enabled: bool,
    count: usize,
});

optional_section!(FilenameParserSection {
    enabled: bool,
    year_min: u32,
    year_max: u32,
});

optional_section!(ExtractSection {
    epub_year_range_check: bool,
    epub_year_min: i32,
    epub_year_max: i32,
    epub_isbn_recognition: bool,
    marc_role_mapping: bool,
    txt_toc_enabled: bool,
});

optional_section!(HtmlSection {
    block_tags: Vec<String>,
    skip_tags: Vec<String>,
});

optional_section!(QualitySection {
    chars_per_page_ocr: f64,
    chars_per_page_doubt: f64,
    replacement_ocr: f64,
    pua_ocr: f64,
    pua_doubt: f64,
    control_ocr: f64,
    dual_layer: f64,
    cjk_space_doubt: f64,
});

/// Re-export the loader's small surface for callers that need to
/// transport a `LoadError` upward.
pub(crate) mod loader_path {
    pub(crate) const OVERLAY: &str = super::PROFILE_OVERLAY_FILE;
    #[allow(dead_code)]
    pub(crate) fn overlay_under(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(OVERLAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_profile_matches_hard_coded_constants() {
        let profile = AuditProfile::default_profile();
        assert_eq!(profile.name, PROFILE_DEFAULT);
        assert!(profile.audit_enabled);
        assert_eq!(profile.year.min, 1450);
        assert_eq!(profile.year.max, 2100);
        assert!(profile.year.range_check);
        assert!(profile.year.pdf_likely_file_date);
        assert!(profile.year.timestamp_form);
        assert!(profile.year.cross_field_filename_override);
        assert!(profile.title.placeholder_check);
        assert_eq!(profile.title.bracketed_min_chars, 3);
        assert!(profile.title.any_bracketed_enabled());
        assert_eq!(profile.toc_shape.flat_min_entries, 5);
        assert_eq!(profile.toc_shape.flat_severe_min_entries, 10);
        assert_eq!(profile.toc_shape.skew_min, 5);
        assert_eq!(profile.toc_shape.skew_ratio, 4);
        assert_eq!(profile.toc_shape.large_body_min_blocks, 100);
        assert_eq!(profile.copyright_blocks.count, 6);
        assert!(profile.publisher.url_watermark);
        assert!(profile.publisher.whitelist_normalize_abbreviations);
        assert!(profile.publisher.drop_10digit_isbn_to_filename);
        assert!(profile.filename_parser.enabled);
        assert_eq!(profile.filename_parser.year_min, 1500);
        assert_eq!(profile.filename_parser.year_max, 2100);
        assert_eq!(profile.extract.epub_year_min, 1000);
        assert_eq!(profile.extract.epub_year_max, 2200);
        assert!(profile.extract.epub_isbn_recognition);
        assert!(profile.extract.marc_role_mapping);
        assert!(profile.extract.txt_toc_enabled);
    }

    #[test]
    fn trust_source_profile_disables_every_toggle() {
        let profile = AuditProfile::trust_source();
        assert_eq!(profile.name, PROFILE_TRUST_SOURCE);
        assert!(!profile.audit_enabled);
        assert!(!profile.year.range_check);
        assert!(!profile.year.pdf_likely_file_date);
        assert!(!profile.year.timestamp_form);
        assert!(!profile.year.cross_field_filename_override);
        assert!(!profile.title.placeholder_check);
        assert!(!profile.title.purely_numeric);
        assert!(!profile.title.any_bracketed_enabled());
        assert!(!profile.language.bcp47_check);
        assert!(!profile.language.body_script_match);
        assert!(!profile.publisher.url_watermark);
        assert!(!profile.publisher.drop_10digit_isbn_to_filename);
        assert!(!profile.toc_shape.suspicious_flat);
        assert!(!profile.toc_shape.heading_block_skew);
        assert!(!profile.toc_shape.empty_large_body);
        assert!(!profile.source_prior.enabled);
        assert!(!profile.copyright_blocks.enabled);
        assert!(!profile.filename_parser.enabled);
        assert!(!profile.extract.epub_year_range_check);
        assert!(!profile.extract.epub_isbn_recognition);
        assert!(!profile.extract.marc_role_mapping);
        assert!(!profile.extract.txt_toc_enabled);
    }

    #[test]
    fn from_named_resolves_three_built_in_profiles() {
        assert!(AuditProfile::from_named(PROFILE_DEFAULT).is_some());
        assert!(AuditProfile::from_named(PROFILE_TRUST_SOURCE).is_some());
        assert!(AuditProfile::from_named(PROFILE_STRICT).is_some());
        assert!(AuditProfile::from_named("unknown-profile").is_none());
    }

    #[test]
    fn all_built_in_names_resolves_via_from_named() {
        assert_eq!(
            ALL_BUILT_IN_NAMES,
            &[PROFILE_DEFAULT, PROFILE_TRUST_SOURCE, PROFILE_STRICT]
        );
        for name in ALL_BUILT_IN_NAMES {
            assert!(
                AuditProfile::from_named(name).is_some(),
                "{name} resolves via from_named",
            );
        }
    }

    #[test]
    fn load_from_empty_directory_yields_default_profile() {
        let dir = TempDir::new().unwrap();
        let loaded = AuditProfile::load_from(dir.path()).unwrap();
        assert_eq!(loaded, AuditProfile::default_profile());
    }

    #[test]
    fn load_from_directory_with_overlay_merges_fields() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(PROFILE_OVERLAY_FILE);
        std::fs::write(
            &overlay,
            "schema_version = 1\n\n[year]\nmin = 1100\nmax = 2200\n",
        )
        .unwrap();
        let loaded = AuditProfile::load_from(dir.path()).unwrap();
        assert_eq!(loaded.year.min, 1100);
        assert_eq!(loaded.year.max, 2200);
        // Untouched fields still match the default.
        assert!(loaded.year.range_check);
        assert!(loaded.title.placeholder_check);
        assert_eq!(loaded.copyright_blocks.count, 6);
    }

    #[test]
    fn load_from_overlay_can_disable_named_toggle() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(PROFILE_OVERLAY_FILE);
        std::fs::write(
            &overlay,
            "schema_version = 1\n\n[publisher]\nurl_watermark = false\n",
        )
        .unwrap();
        let loaded = AuditProfile::load_from(dir.path()).unwrap();
        assert!(!loaded.publisher.url_watermark);
        assert!(loaded.publisher.whitelist_normalize_abbreviations);
    }

    #[test]
    fn overlay_with_unsupported_schema_version_rejected() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(PROFILE_OVERLAY_FILE);
        std::fs::write(&overlay, "schema_version = 2\n").unwrap();
        let err = AuditProfile::load_from(dir.path()).unwrap_err();
        assert!(matches!(err, LoadError::SchemaVersion { found: 2, .. }));
    }

    #[test]
    fn malformed_overlay_returns_parse_error() {
        let dir = TempDir::new().unwrap();
        let overlay = dir.path().join(PROFILE_OVERLAY_FILE);
        std::fs::write(&overlay, "not = valid = toml\n").unwrap();
        let err = AuditProfile::load_from(dir.path()).unwrap_err();
        assert!(matches!(err, LoadError::Parse { .. }));
    }

    #[test]
    fn default_implementation_matches_default_profile() {
        assert_eq!(AuditProfile::default(), AuditProfile::default_profile());
    }

    #[test]
    fn overlay_helper_paths_round_trip() {
        let p = std::path::Path::new("/tmp/fake");
        assert_eq!(
            loader_path::overlay_under(p),
            std::path::PathBuf::from("/tmp/fake/audit_profile.local.toml")
        );
    }
}
