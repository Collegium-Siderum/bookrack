// SPDX-License-Identifier: Apache-2.0

//! `book.toml` shape for the distill loader.
//!
//! Phase 4 ships the minimum the cross-catalog validator needs:
//! the slug, the parser section's declared `writes_properties`, and
//! the stage chain (each entry is either a bare stage name or a table
//! with `stage = "..."` plus parameter keys). Phase 7 will extend
//! this with `[[indexes]]`, the loader (`load(path)`), and the
//! catalog-driven `into_pipeline`.

use std::collections::BTreeMap;

use serde::Deserialize;
use toml::Value as TomlValue;

use crate::error::ParseError;

#[derive(Debug, Clone, Deserialize)]
pub struct BookToml {
    pub book_slug: String,
    pub schema_name: String,
    pub schema_version: i64,
    pub parser_version: String,
    #[serde(default)]
    pub authority_rank: i64,
    pub parser: ParserSection,
    #[serde(default)]
    pub indexes: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ParserSection {
    pub writes_properties: Vec<String>,
    pub stages: Vec<StageRef>,
}

/// One entry of `parser.stages`. TOML allows either a bare string
/// for parameterless stages or an inline table with `stage = "..."`
/// plus the stage's parameter keys.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StageRef {
    Bare(String),
    Configured(StageConfig),
}

#[derive(Debug, Clone, Deserialize)]
pub struct StageConfig {
    pub stage: String,
    #[serde(flatten)]
    pub params: BTreeMap<String, TomlValue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexEntry {
    pub field: String,
    #[serde(default = "default_index_kind")]
    pub kind: String,
}

fn default_index_kind() -> String {
    "btree".to_string()
}

impl StageRef {
    /// Canonical stage name, regardless of bare-string or
    /// configured-table shape.
    pub fn name(&self) -> &str {
        match self {
            StageRef::Bare(s) => s.as_str(),
            StageRef::Configured(cfg) => cfg.stage.as_str(),
        }
    }

    /// Live parameter values for a configured stage; an empty map
    /// for a bare-string ref.
    pub fn params(&self) -> Option<&BTreeMap<String, TomlValue>> {
        match self {
            StageRef::Bare(_) => None,
            StageRef::Configured(cfg) => Some(&cfg.params),
        }
    }
}

impl BookToml {
    /// Parse the file's TOML text. Phase 7 adds a `load(path)`
    /// that reads from disk.
    pub fn parse_str(s: &str) -> Result<Self, ParseError> {
        toml::from_str(s).map_err(|e| ParseError::TomlParse(e.to_string()))
    }
}
