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
use std::path::Path;

use serde::Deserialize;
use toml::Value as TomlValue;

use crate::catalogs::Catalogs;
use crate::dispatch::dispatch_stage;
use crate::error::ParseError;
use crate::pipeline::Pipeline;

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
    /// Parse the file's TOML text.
    pub fn parse_str(s: &str) -> Result<Self, ParseError> {
        toml::from_str(s).map_err(|e| ParseError::TomlParse(e.to_string()))
    }

    /// Read and parse `book.toml` from disk.
    pub fn load(path: &Path) -> Result<Self, ParseError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ParseError::TomlParse(format!("read {}: {e}", path.display())))?;
        Self::parse_str(&text)
    }

    /// Run the cross-catalog validator and, on success, build a
    /// runnable [`Pipeline`] by dispatching each `parser.stages`
    /// entry through [`dispatch_stage`].
    pub fn into_pipeline(&self, catalogs: &Catalogs) -> Result<Pipeline, ParseError> {
        catalogs.validate_book(self)?;
        let mut pipeline = Pipeline::new(self.book_slug.clone());
        for stage_ref in &self.parser.stages {
            let stage = dispatch_stage(stage_ref.name(), stage_ref.params())?;
            pipeline.push(stage);
        }
        Ok(pipeline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete book.toml that exercises every dispatch branch
    /// downstream of `walk_anchors` so `into_pipeline` actually
    /// constructs each builtin.
    const LEGAL_BOOK: &str = r#"
book_slug      = "name_translation_xinhua"
schema_name    = "name_translation"
schema_version = 1
parser_version = "0.1.0"
authority_rank = 10

[parser]
writes_properties = [
  "chinese_name", "year_span", "country", "gender",
  "variants", "bio_annotation", "redirect_to",
]
stages = [
  "split_pages",
  { stage = "one_block_per_page", lang = "latin" },
  { stage = "walk_anchors",
    anchor = "latin_headword",
    reject = ["running_header"],
    drop_lone_letter_dividers = true,
    splice_orphans_to_prev_block = true },
  "split_at_first_cjk",
  { stage = "extract_year_span", payload_key = "year_span" },
  { stage = "extract_gender_tag", payload_key = "gender" },
  { stage = "partition_body_around_match",
    pattern = { bracketed_tag = ["angle", "square"] },
    head_payload_key = "country",
    head_split_by = "[;]",
    first_to = "chinese_name",
    rest_to  = "variants",
    tail_to  = "bio_annotation" },
  { stage = "to_entry_draft",
    key_normalizer = "normalize_latin_key",
    aliases_from_payload = ["chinese_name", "variants"],
    carry_payload_keys = [
      "chinese_name", "year_span", "country",
      "gender", "variants", "bio_annotation",
    ] },
]

[[indexes]]
field = "country"
kind  = "btree"

[[indexes]]
field = "year_span.birth"
"#;

    fn catalogs() -> Catalogs {
        Catalogs::load_all().expect("load_all")
    }

    #[test]
    fn a_legal_book_loads_and_into_pipeline_builds_every_stage() {
        let book = BookToml::parse_str(LEGAL_BOOK).expect("parse legal book.toml");
        let pipeline = book
            .into_pipeline(&catalogs())
            .expect("into_pipeline on legal book");
        // Eight stages: split_pages, one_block_per_page, walk_anchors,
        // split_at_first_cjk, extract_year_span, extract_gender_tag,
        // partition_body_around_match, to_entry_draft.
        assert_eq!(pipeline.len(), 8);
        assert_eq!(pipeline.name(), "name_translation_xinhua");
        // Both declared indexes survive the deserialize.
        assert_eq!(book.indexes.len(), 2);
        assert_eq!(book.indexes[0].field, "country");
        assert_eq!(book.indexes[0].kind, "btree");
        // `[[indexes]]` with no `kind = ...` falls back to "btree".
        assert_eq!(book.indexes[1].field, "year_span.birth");
        assert_eq!(book.indexes[1].kind, "btree");
    }

    #[test]
    fn unknown_stage_in_book_toml_raises_stage_not_found() {
        let toml = LEGAL_BOOK.replace("\"split_pages\"", "\"non_existent_stage\"");
        let book = BookToml::parse_str(&toml).unwrap();
        match book.into_pipeline(&catalogs()).unwrap_err() {
            ParseError::StageNotFound(name) => assert_eq!(name, "non_existent_stage"),
            other => panic!("expected StageNotFound, got {other:?}"),
        }
    }

    #[test]
    fn script_escape_hatch_raises_script_ref_forbidden_with_manual_citation() {
        let toml = LEGAL_BOOK.replace("\"split_pages\"", "\"@script::foo\"");
        let book = BookToml::parse_str(&toml).unwrap();
        let err = book.into_pipeline(&catalogs()).unwrap_err();
        match &err {
            ParseError::ScriptRefForbidden(name) => assert_eq!(name, "foo"),
            other => panic!("expected ScriptRefForbidden, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("§1.4"),
            "ScriptRefForbidden must cite manual §1.4, got: {msg}"
        );
    }

    #[test]
    fn llm_hook_reference_raises_llm_hook_not_implemented_with_section_citation() {
        let toml = LEGAL_BOOK.replace("\"split_pages\"", "\"@llm::bar\"");
        let book = BookToml::parse_str(&toml).unwrap();
        let err = book.into_pipeline(&catalogs()).unwrap_err();
        match &err {
            ParseError::LlmHookNotImplemented(name) => assert_eq!(name, "bar"),
            other => panic!("expected LlmHookNotImplemented, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("§8.1"),
            "LlmHookNotImplemented must cite mother doc §8.1, got: {msg}"
        );
    }

    #[test]
    fn unknown_writes_properties_raises_catalog_violation() {
        let toml = LEGAL_BOOK.replace(
            "\"chinese_name\", \"year_span\"",
            "\"random_key\", \"year_span\"",
        );
        let book = BookToml::parse_str(&toml).unwrap();
        match book.into_pipeline(&catalogs()).unwrap_err() {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("random_key") && msg.contains("property_catalog.toml"),
                    "violation must name the bad key and the catalog: {msg}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }

    #[test]
    fn unknown_anchor_rule_raises_catalog_violation_from_dispatch() {
        // The catalog validator can't catch this — the anchor param
        // is opaque at validate time — so this surfaces during
        // dispatch.
        let toml = LEGAL_BOOK.replace("anchor = \"latin_headword\"", "anchor = \"made_up_rule\"");
        let book = BookToml::parse_str(&toml).unwrap();
        match book.into_pipeline(&catalogs()).unwrap_err() {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("made_up_rule"),
                    "violation must name the bad anchor rule: {msg}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }

    #[test]
    fn load_pipeline_round_trip_can_run_a_synthetic_source() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("book.toml");
        std::fs::write(&path, LEGAL_BOOK).expect("write fixture book.toml");
        let pipeline = crate::load_pipeline(&path).expect("load_pipeline");
        assert_eq!(pipeline.len(), 8);
        // Smoke a minimal source through the assembled pipeline; the
        // shape of the output is exercised more thoroughly in phase 8.
        let source = "<!-- page 1 (sheet 1) -->\nSmith\n1900-2000\nan american baseball player\n";
        let (drafts, coverage) = pipeline.run(source.to_string()).expect("pipeline run");
        let _ = writeln!(
            std::io::sink(),
            "synthetic pipeline drafted {} entries / pages={}",
            drafts.len(),
            coverage.pages,
        );
    }
}
