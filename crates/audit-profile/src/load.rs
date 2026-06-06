// SPDX-License-Identifier: Apache-2.0

//! Loader for [`crate::AuditProfile`]. Parses the schema-locked
//! default and any runtime overlay, then merges them field-by-field.

use std::path::{Path, PathBuf};

use crate::{AuditProfile, ProfileFile, SCHEMA_VERSION};

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

/// Parse a complete profile TOML and assign it the given `name`.
pub(crate) fn parse_str(toml: &str, name: &str) -> Result<AuditProfile, LoadError> {
    // The shipped default is parsed through the same path as an
    // overlay, so the synthetic path is only used inside an error.
    let synthetic = PathBuf::from(format!("<embedded:{name}>"));
    let file: ProfileFile = toml::from_str(toml).map_err(|error| LoadError::Parse {
        path: synthetic.clone(),
        error,
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(LoadError::SchemaVersion {
            path: synthetic,
            found: file.schema_version,
        });
    }
    let mut profile = AuditProfile {
        name: name.to_string(),
        audit_enabled: true,
        year: Default::default(),
        title: Default::default(),
        language: Default::default(),
        publisher: Default::default(),
        toc_shape: Default::default(),
        source_prior: Default::default(),
        copyright_blocks: Default::default(),
        filename_parser: Default::default(),
        extract: Default::default(),
        html: Default::default(),
        quality: Default::default(),
    };
    apply_overlay(&mut profile, file);
    Ok(profile)
}

/// Parse an overlay file and merge its declared fields into `profile`.
pub(crate) fn merge_overlay(
    profile: &mut AuditProfile,
    raw: &str,
    path: &Path,
) -> Result<(), LoadError> {
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

fn apply_overlay(profile: &mut AuditProfile, file: ProfileFile) {
    if let Some(v) = file.audit_enabled {
        profile.audit_enabled = v;
    }
    if let Some(s) = file.year {
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
        if let Some(v) = s.cross_field_filename_override {
            profile.year.cross_field_filename_override = v;
        }
    }
    if let Some(s) = file.title {
        if let Some(v) = s.placeholder_check {
            profile.title.placeholder_check = v;
        }
        if let Some(v) = s.purely_numeric {
            profile.title.purely_numeric = v;
        }
        if let Some(v) = s.series_paren {
            profile.title.series_paren = v;
        }
        if let Some(v) = s.marketing_block {
            profile.title.marketing_block = v;
        }
        if let Some(v) = s.aggregator_marker {
            profile.title.aggregator_marker = v;
        }
        if let Some(v) = s.volume_marker {
            profile.title.volume_marker = v;
        }
        if let Some(v) = s.bracketed_min_chars {
            profile.title.bracketed_min_chars = v;
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
    if let Some(s) = file.publisher {
        if let Some(v) = s.url_watermark {
            profile.publisher.url_watermark = v;
        }
        if let Some(v) = s.whitelist_normalize_abbreviations {
            profile.publisher.whitelist_normalize_abbreviations = v;
        }
        if let Some(v) = s.drop_10digit_isbn_to_filename {
            profile.publisher.drop_10digit_isbn_to_filename = v;
        }
    }
    if let Some(s) = file.toc_shape {
        if let Some(v) = s.suspicious_flat {
            profile.toc_shape.suspicious_flat = v;
        }
        if let Some(v) = s.flat_min_entries {
            profile.toc_shape.flat_min_entries = v;
        }
        if let Some(v) = s.flat_severe_min_entries {
            profile.toc_shape.flat_severe_min_entries = v;
        }
        if let Some(v) = s.heading_block_skew {
            profile.toc_shape.heading_block_skew = v;
        }
        if let Some(v) = s.skew_min {
            profile.toc_shape.skew_min = v;
        }
        if let Some(v) = s.skew_ratio {
            profile.toc_shape.skew_ratio = v;
        }
        if let Some(v) = s.empty_large_body {
            profile.toc_shape.empty_large_body = v;
        }
        if let Some(v) = s.large_body_min_blocks {
            profile.toc_shape.large_body_min_blocks = v;
        }
    }
    if let Some(s) = file.source_prior
        && let Some(v) = s.enabled
    {
        profile.source_prior.enabled = v;
    }
    if let Some(s) = file.copyright_blocks {
        if let Some(v) = s.enabled {
            profile.copyright_blocks.enabled = v;
        }
        if let Some(v) = s.count {
            profile.copyright_blocks.count = v;
        }
    }
    if let Some(s) = file.filename_parser {
        if let Some(v) = s.enabled {
            profile.filename_parser.enabled = v;
        }
        if let Some(v) = s.year_min {
            profile.filename_parser.year_min = v;
        }
        if let Some(v) = s.year_max {
            profile.filename_parser.year_max = v;
        }
    }
    if let Some(s) = file.extract {
        if let Some(v) = s.epub_year_range_check {
            profile.extract.epub_year_range_check = v;
        }
        if let Some(v) = s.epub_year_min {
            profile.extract.epub_year_min = v;
        }
        if let Some(v) = s.epub_year_max {
            profile.extract.epub_year_max = v;
        }
        if let Some(v) = s.epub_isbn_recognition {
            profile.extract.epub_isbn_recognition = v;
        }
        if let Some(v) = s.marc_role_mapping {
            profile.extract.marc_role_mapping = v;
        }
        if let Some(v) = s.txt_toc_enabled {
            profile.extract.txt_toc_enabled = v;
        }
    }
    if let Some(s) = file.html {
        if let Some(v) = s.block_tags {
            profile.html.block_tags = v;
        }
        if let Some(v) = s.skip_tags {
            profile.html.skip_tags = v;
        }
    }
    if let Some(s) = file.quality {
        if let Some(v) = s.chars_per_page_ocr {
            profile.quality.chars_per_page_ocr = v as u32;
        }
        if let Some(v) = s.chars_per_page_doubt {
            profile.quality.chars_per_page_doubt = v as u32;
        }
        if let Some(v) = s.replacement_ocr {
            profile.quality.replacement_ocr_bp = (v * 10_000.0) as u32;
        }
        if let Some(v) = s.pua_ocr {
            profile.quality.pua_ocr_bp = (v * 10_000.0) as u32;
        }
        if let Some(v) = s.pua_doubt {
            profile.quality.pua_doubt_bp = (v * 10_000.0) as u32;
        }
        if let Some(v) = s.control_ocr {
            profile.quality.control_ocr_bp = (v * 10_000.0) as u32;
        }
        if let Some(v) = s.dual_layer {
            profile.quality.dual_layer_bp = (v * 10_000.0) as u32;
        }
        if let Some(v) = s.cjk_space_doubt {
            profile.quality.cjk_space_doubt_bp = (v * 10_000.0) as u32;
        }
    }
}
