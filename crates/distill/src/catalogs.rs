// SPDX-License-Identifier: Apache-2.0

//! The three controlled vocabularies that gate the distill pipeline.
//!
//! * `property_catalog.toml` — keys allowed in `payload_json`.
//! * `quality_flags.toml` — flag names a stage may stamp on an entry.
//! * `stage_catalog.toml` — every builtin stage's public surface.
//!
//! All three travel in-repo under `crates/distill/data/`; the loader
//! embeds them with `include_str!`. [`Catalogs::load_all`] parses,
//! cross-checks the catalogs against each other (so a stage cannot
//! declare a `writes_properties` key the property catalog has never
//! heard of, etc.), and returns the live `Catalogs`. Per-book
//! validation runs through [`Catalogs::validate_book`].

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use toml::Value as TomlValue;

use crate::book_toml::{BookToml, StageRef};
use crate::error::ParseError;

const PROPERTY_CATALOG_TOML: &str = include_str!("../data/property_catalog.toml");
const QUALITY_FLAGS_TOML: &str = include_str!("../data/quality_flags.toml");
const STAGE_CATALOG_TOML: &str = include_str!("../data/stage_catalog.toml");

/// Recognized `StageData` variant names. Cross-checked against
/// `stage_catalog.toml`'s `input` / `output` fields at startup.
const STAGE_DATA_KINDS: &[&str] = &["source", "pages", "blocks", "raws", "splits", "drafts"];

// ---------------------------------------------------------------------------
// Public, ergonomic catalog types
// ---------------------------------------------------------------------------

/// The whole catalog triad, validated against each other.
#[derive(Debug, Clone)]
pub struct Catalogs {
    pub properties: PropertyCatalog,
    pub quality_flags: QualityFlagCatalog,
    pub stages: StageCatalog,
}

#[derive(Debug, Clone)]
pub struct PropertyCatalog {
    pub entries: BTreeMap<String, PropertySpec>,
}

#[derive(Debug, Clone)]
pub struct PropertySpec {
    pub description: Option<String>,
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct QualityFlagCatalog {
    pub entries: BTreeMap<String, FlagSpec>,
}

#[derive(Debug, Clone)]
pub struct FlagSpec {
    pub severity: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct StageCatalog {
    pub entries: BTreeMap<String, StageSpec>,
}

#[derive(Debug, Clone)]
pub struct StageSpec {
    pub input: String,
    pub output: String,
    pub description: Option<String>,
    pub writes_properties: Vec<String>,
    pub emits_flags: Vec<String>,
    pub params: Vec<ParamSpec>,
}

#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: String,
    pub type_: String,
    pub required: bool,
    pub default: Option<TomlValue>,
}

// ---------------------------------------------------------------------------
// On-disk shapes (private; converted into the public types above)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PropertyCatalogToml {
    #[allow(dead_code)]
    schema_version: i64,
    #[serde(flatten)]
    entries: BTreeMap<String, PropertySpecToml>,
}

#[derive(Debug, Deserialize)]
struct PropertySpecToml {
    #[allow(dead_code)]
    #[serde(rename = "type", default)]
    type_: Option<TomlValue>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    used_by: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct QualityFlagCatalogToml {
    #[allow(dead_code)]
    schema_version: i64,
    #[serde(flatten)]
    entries: BTreeMap<String, FlagSpecToml>,
}

#[derive(Debug, Deserialize)]
struct FlagSpecToml {
    severity: String,
    description: String,
    #[serde(default)]
    #[allow(dead_code)]
    sourced_from: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StageCatalogToml {
    #[allow(dead_code)]
    schema_version: i64,
    #[serde(flatten)]
    entries: BTreeMap<String, StageSpecToml>,
}

#[derive(Debug, Deserialize)]
struct StageSpecToml {
    input: String,
    output: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    writes_properties: Vec<String>,
    #[serde(default)]
    emits_flags: Vec<String>,
    #[serde(default)]
    params: Vec<ParamSpecToml>,
}

#[derive(Debug, Deserialize)]
struct ParamSpecToml {
    name: String,
    #[serde(rename = "type")]
    type_: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<TomlValue>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl Catalogs {
    /// Parse all three repo-embedded catalogs and run the
    /// cross-catalog self-check.
    pub fn load_all() -> Result<Self, ParseError> {
        Self::load_from(
            PROPERTY_CATALOG_TOML,
            QUALITY_FLAGS_TOML,
            STAGE_CATALOG_TOML,
        )
    }

    /// Stable fingerprint of the three embedded catalog TOMLs, in the
    /// order (properties, quality flags, stages). Stamped into
    /// `book_distill_audit.profile_ref` by the build audit writer, so
    /// an audit row records which controlled vocabularies judged it.
    pub fn embedded_fingerprint() -> String {
        bookrack_audit_profile::stable_fingerprint_parts(&[
            PROPERTY_CATALOG_TOML.as_bytes(),
            QUALITY_FLAGS_TOML.as_bytes(),
            STAGE_CATALOG_TOML.as_bytes(),
        ])
        .expect("embedded catalog TOMLs must fingerprint")
    }

    /// Readable summary of the embedded quality-flag vocabulary,
    /// paired with [`Catalogs::embedded_fingerprint`] on the audit row.
    pub fn embedded_flag_summary() -> String {
        Self::load_all()
            .expect("embedded catalog TOMLs must parse")
            .quality_flag_summary()
    }

    /// Summarize this catalog set's quality-flag vocabulary as a JSON
    /// array of `{"name", "severity"}` objects sorted by name — the
    /// distill-side analog of a profile's boolean-toggle summary.
    pub fn quality_flag_summary(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .quality_flags
            .entries
            .iter()
            .map(|(name, spec)| {
                // Keys are inserted in sorted order so the byte output
                // does not depend on the JSON map backend.
                let mut entry = serde_json::Map::new();
                entry.insert("name".to_string(), serde_json::Value::String(name.clone()));
                entry.insert(
                    "severity".to_string(),
                    serde_json::Value::String(spec.severity.clone()),
                );
                serde_json::Value::Object(entry)
            })
            .collect();
        serde_json::Value::Array(entries).to_string()
    }

    /// Parse the three catalogs from arbitrary TOML strings.
    /// Reserved for tests that exercise the self-check against
    /// hand-crafted negative fixtures.
    pub fn load_from(
        properties_toml: &str,
        quality_flags_toml: &str,
        stage_catalog_toml: &str,
    ) -> Result<Self, ParseError> {
        let prop: PropertyCatalogToml = toml::from_str(properties_toml)
            .map_err(|e| ParseError::TomlParse(format!("property_catalog.toml: {e}")))?;
        let flag: QualityFlagCatalogToml = toml::from_str(quality_flags_toml)
            .map_err(|e| ParseError::TomlParse(format!("quality_flags.toml: {e}")))?;
        let stage: StageCatalogToml = toml::from_str(stage_catalog_toml)
            .map_err(|e| ParseError::TomlParse(format!("stage_catalog.toml: {e}")))?;

        let catalogs = Catalogs {
            properties: PropertyCatalog {
                entries: prop
                    .entries
                    .into_iter()
                    .map(|(k, v)| {
                        (
                            k,
                            PropertySpec {
                                description: v.description,
                                used_by: v.used_by,
                            },
                        )
                    })
                    .collect(),
            },
            quality_flags: QualityFlagCatalog {
                entries: flag
                    .entries
                    .into_iter()
                    .map(|(k, v)| {
                        (
                            k,
                            FlagSpec {
                                severity: v.severity,
                                description: v.description,
                            },
                        )
                    })
                    .collect(),
            },
            stages: StageCatalog {
                entries: stage
                    .entries
                    .into_iter()
                    .map(|(k, v)| {
                        (
                            k,
                            StageSpec {
                                input: v.input,
                                output: v.output,
                                description: v.description,
                                writes_properties: v.writes_properties,
                                emits_flags: v.emits_flags,
                                params: v
                                    .params
                                    .into_iter()
                                    .map(|p| ParamSpec {
                                        name: p.name,
                                        type_: p.type_,
                                        required: p.required,
                                        default: p.default,
                                    })
                                    .collect(),
                            },
                        )
                    })
                    .collect(),
            },
        };

        catalogs.self_check()?;
        Ok(catalogs)
    }

    // ----- self-check ----------------------------------------------------

    /// Cross-catalog invariants: stage declarations must reference
    /// only properties and flags the other catalogs know about, and
    /// `input` / `output` must name a real `StageData` variant.
    fn self_check(&self) -> Result<(), ParseError> {
        for (name, spec) in &self.stages.entries {
            if !STAGE_DATA_KINDS.contains(&spec.input.as_str()) {
                return Err(ParseError::CatalogViolation(format!(
                    "stage_catalog stage {name:?} declares input={:?} \
                     which is not a known StageData variant",
                    spec.input
                )));
            }
            if !STAGE_DATA_KINDS.contains(&spec.output.as_str()) {
                return Err(ParseError::CatalogViolation(format!(
                    "stage_catalog stage {name:?} declares output={:?} \
                     which is not a known StageData variant",
                    spec.output
                )));
            }
            for prop in &spec.writes_properties {
                if !self.properties.entries.contains_key(prop) {
                    return Err(ParseError::CatalogViolation(format!(
                        "stage_catalog stage {name:?} declares writes_properties \
                         {prop:?} which is not in property_catalog.toml"
                    )));
                }
            }
            for flag in &spec.emits_flags {
                if !self.quality_flags.entries.contains_key(flag) {
                    return Err(ParseError::CatalogViolation(format!(
                        "stage_catalog stage {name:?} declares emits_flags \
                         {flag:?} which is not in quality_flags.toml"
                    )));
                }
            }
        }
        Ok(())
    }

    // ----- per-book validation ------------------------------------------

    /// Run every book-level rule against a parsed `book.toml`. The
    /// rule numbering mirrors the eight listed in §3 phase 4 of the
    /// v2 distill execution manual.
    pub fn validate_book(&self, book: &BookToml) -> Result<(), ParseError> {
        // Rule 8: slug grammar. The slug is the authority segment of
        // refs:// URIs, whose parser splits at the first '#'; the closed
        // character set keeps that split unambiguous.
        if book.book_slug.is_empty()
            || !book
                .book_slug
                .chars()
                .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
        {
            return Err(ParseError::CatalogViolation(format!(
                "book slug {:?} must be non-empty and match [a-z0-9_-]+",
                book.book_slug
            )));
        }

        // Rule 4: declared writes_properties ⊆ property_catalog.
        for prop in &book.parser.writes_properties {
            if !self.properties.entries.contains_key(prop) {
                return Err(ParseError::CatalogViolation(format!(
                    "book {:?} declares parser.writes_properties {prop:?} \
                     which is not in property_catalog.toml",
                    book.book_slug
                )));
            }
        }

        let book_writes: BTreeSet<&str> = book
            .parser
            .writes_properties
            .iter()
            .map(String::as_str)
            .collect();

        let mut chain_writes: BTreeSet<&str> = BTreeSet::new();

        for stage_ref in &book.parser.stages {
            let stage_name = stage_ref.name();

            // Rule 2: forbidden @script:: escape hatch.
            if let Some(rest) = stage_name.strip_prefix("@script::") {
                return Err(ParseError::ScriptRefForbidden(rest.to_string()));
            }
            // Rule 3: deferred @llm:: hook.
            if let Some(rest) = stage_name.strip_prefix("@llm::") {
                return Err(ParseError::LlmHookNotImplemented(rest.to_string()));
            }

            // Rule 1: the stage must be registered.
            let spec = self
                .stages
                .entries
                .get(stage_name)
                .ok_or_else(|| ParseError::StageNotFound(stage_name.to_string()))?;

            for prop in &spec.writes_properties {
                chain_writes.insert(prop.as_str());
            }

            self.validate_stage_params(book, stage_ref, spec)?;
        }

        // Rule 5: union of stage-declared writes ⊆ book-declared
        // writes_properties.
        for prop in &chain_writes {
            if !book_writes.contains(prop) {
                return Err(ParseError::CatalogViolation(format!(
                    "book {:?} stage chain writes property {prop:?} \
                     but parser.writes_properties does not declare it",
                    book.book_slug
                )));
            }
        }

        Ok(())
    }

    /// Per-stage parameter checks: required-presence and the
    /// `quality_flag_ref` value lookup against the quality_flag
    /// catalog (rules 6 and 7 in the manual).
    fn validate_stage_params(
        &self,
        book: &BookToml,
        stage_ref: &StageRef,
        spec: &StageSpec,
    ) -> Result<(), ParseError> {
        let params = stage_ref.params();
        let stage_name = stage_ref.name();
        for param in &spec.params {
            let value = params.and_then(|p| p.get(&param.name));

            // Rule 7: required params must be present.
            if param.required && value.is_none() {
                return Err(ParseError::CatalogViolation(format!(
                    "book {:?} stage {stage_name:?} is missing required \
                     param {:?} (declared in stage_catalog.toml)",
                    book.book_slug, param.name
                )));
            }

            // Rule 6: a `quality_flag_ref` param value must be a
            // catalog-known flag name.
            if param.type_ == "quality_flag_ref"
                && let Some(value) = value
                && let Some(flag) = value.as_str()
                && !self.quality_flags.entries.contains_key(flag)
            {
                return Err(ParseError::CatalogViolation(format!(
                    "book {:?} stage {stage_name:?} param {:?} = {flag:?} \
                     is not in quality_flags.toml",
                    book.book_slug, param.name
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book_toml::BookToml;

    /// One legal book.toml that exercises every catalog wiring: two
    /// stages with required + optional params, a `writes_properties`
    /// declaration that covers what the chain actually writes.
    const VALID_BOOK_TOML: &str = r#"
book_slug      = "fake_book"
schema_name    = "name_translation"
schema_version = 1
parser_version = "0.1.0"
authority_rank = 10

[parser]
writes_properties = ["year_span", "gender"]
stages = [
  "split_pages",
  { stage = "one_block_per_page", lang = "latin" },
  { stage = "walk_anchors", anchor = "latin_headword" },
  "split_at_first_cjk",
  { stage = "extract_year_span", payload_key = "year_span" },
  { stage = "extract_gender_tag", payload_key = "gender" },
  { stage = "to_entry_draft", key_normalizer = "normalize_latin_key" },
]
"#;

    #[test]
    fn load_all_parses_the_three_catalogs_with_non_empty_descriptions() {
        let cats = Catalogs::load_all().expect("load_all");
        assert!(
            cats.properties.entries.contains_key("country"),
            "property catalog must contain `country`"
        );
        assert!(
            cats.quality_flags.entries.contains_key("pair_mismatch"),
            "quality_flag catalog must contain `pair_mismatch`"
        );
        assert!(
            cats.stages.entries.contains_key("split_pages"),
            "stage catalog must contain `split_pages`"
        );

        let prop = &cats.properties.entries["country"];
        assert!(
            prop.description.as_deref().is_some_and(|d| !d.is_empty()),
            "country property must carry a non-empty description"
        );
        let flag = &cats.quality_flags.entries["pair_mismatch"];
        assert!(
            !flag.description.is_empty(),
            "pair_mismatch flag must carry a non-empty description"
        );
        let stage = &cats.stages.entries["walk_anchors"];
        assert!(
            stage.description.as_deref().is_some_and(|d| !d.is_empty()),
            "walk_anchors stage must carry a non-empty description"
        );
    }

    #[test]
    fn embedded_fingerprint_is_stable_short_hex() {
        let fp = Catalogs::embedded_fingerprint();
        assert_eq!(fp, Catalogs::embedded_fingerprint());
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_well_formed_book_toml_validates_clean() {
        let cats = Catalogs::load_all().expect("load_all");
        let book = BookToml::parse_str(VALID_BOOK_TOML).expect("parse valid book.toml");
        cats.validate_book(&book).expect("validate clean book");
    }

    #[test]
    fn slug_outside_the_closed_character_set_raises_catalog_violation() {
        let cats = Catalogs::load_all().expect("load_all");
        for bad_slug in ["Fake_Book", "fake#book", "fake book", "", "\u{4e66}"] {
            let toml = VALID_BOOK_TOML.replace("\"fake_book\"", &format!("{bad_slug:?}"));
            let book = BookToml::parse_str(&toml).unwrap();
            match cats.validate_book(&book).unwrap_err() {
                ParseError::CatalogViolation(msg) => {
                    assert!(
                        msg.contains(&format!("{bad_slug:?}")),
                        "violation message must quote the offending slug {bad_slug:?}: {msg}"
                    );
                }
                other => panic!("expected CatalogViolation for {bad_slug:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_stage_raises_stage_not_found() {
        let cats = Catalogs::load_all().expect("load_all");
        let toml = VALID_BOOK_TOML.replace("\"split_pages\"", "\"non_existent_stage\"");
        let book = BookToml::parse_str(&toml).unwrap();
        match cats.validate_book(&book).unwrap_err() {
            ParseError::StageNotFound(name) => {
                assert_eq!(name, "non_existent_stage");
            }
            other => panic!("expected StageNotFound, got {other:?}"),
        }
    }

    #[test]
    fn script_escape_hatch_raises_script_ref_forbidden_and_cites_section() {
        let cats = Catalogs::load_all().expect("load_all");
        let toml = VALID_BOOK_TOML.replace("\"split_pages\"", "\"@script::foo\"");
        let book = BookToml::parse_str(&toml).unwrap();
        let err = cats.validate_book(&book).unwrap_err();
        match &err {
            ParseError::ScriptRefForbidden(name) => assert_eq!(name, "foo"),
            other => panic!("expected ScriptRefForbidden, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("§1.4"),
            "ScriptRefForbidden message must cite manual §1.4, got: {msg}"
        );
    }

    #[test]
    fn llm_hook_reference_raises_llm_hook_not_implemented_and_cites_section() {
        let cats = Catalogs::load_all().expect("load_all");
        let toml = VALID_BOOK_TOML.replace("\"split_pages\"", "\"@llm::bar\"");
        let book = BookToml::parse_str(&toml).unwrap();
        let err = cats.validate_book(&book).unwrap_err();
        match &err {
            ParseError::LlmHookNotImplemented(name) => assert_eq!(name, "bar"),
            other => panic!("expected LlmHookNotImplemented, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("§8.1"),
            "LlmHookNotImplemented message must cite mother doc §8.1, got: {msg}"
        );
    }

    #[test]
    fn unknown_writes_properties_raises_catalog_violation_against_property_catalog() {
        let cats = Catalogs::load_all().expect("load_all");
        let toml = VALID_BOOK_TOML.replace(
            "writes_properties = [\"year_span\", \"gender\"]",
            "writes_properties = [\"random_key\"]",
        );
        let book = BookToml::parse_str(&toml).unwrap();
        match cats.validate_book(&book).unwrap_err() {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("random_key") && msg.contains("property_catalog.toml"),
                    "violation message must name the bad key and the catalog: {msg}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }

    #[test]
    fn stage_chain_writing_undeclared_property_raises_catalog_violation() {
        let cats = Catalogs::load_all().expect("load_all");
        // Drop "gender" from the book's writes_properties; the stage
        // chain still references `extract_gender_tag` which declares
        // it. Rule 5 catches the discrepancy.
        let toml = VALID_BOOK_TOML.replace(
            "writes_properties = [\"year_span\", \"gender\"]",
            "writes_properties = [\"year_span\"]",
        );
        let book = BookToml::parse_str(&toml).unwrap();
        match cats.validate_book(&book).unwrap_err() {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("gender") && msg.contains("parser.writes_properties"),
                    "violation message must point at the undeclared property: {msg}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }

    #[test]
    fn quality_flag_ref_value_not_in_catalog_raises_catalog_violation() {
        let cats = Catalogs::load_all().expect("load_all");
        // pair_bilingual_entries carries a `mismatch_flag` param of
        // type `quality_flag_ref`. Feeding a string that is not in
        // the quality_flag catalog is rule 6.
        let toml = r#"
book_slug      = "fake_book"
schema_name    = "name_translation"
schema_version = 1
parser_version = "0.1.0"

[parser]
writes_properties = []
stages = [
  { stage = "pair_bilingual_entries", primary_lang = "en", secondary_lang = "zh", merge_key = "translation", mismatch_flag = "made_up_flag" },
]
"#;
        let book = BookToml::parse_str(toml).unwrap();
        match cats.validate_book(&book).unwrap_err() {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("made_up_flag") && msg.contains("quality_flags.toml"),
                    "violation message must name the bad flag and the catalog: {msg}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_param_raises_catalog_violation() {
        let cats = Catalogs::load_all().expect("load_all");
        // `walk_anchors` requires `anchor`. Reference it as a bare
        // string with no params at all.
        let toml = r#"
book_slug      = "fake_book"
schema_name    = "name_translation"
schema_version = 1
parser_version = "0.1.0"

[parser]
writes_properties = []
stages = ["walk_anchors"]
"#;
        let book = BookToml::parse_str(toml).unwrap();
        match cats.validate_book(&book).unwrap_err() {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("anchor") && msg.contains("required"),
                    "violation message must name the missing param: {msg}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }

    #[test]
    fn self_check_rejects_stage_writing_unknown_property() {
        // Forge a stage_catalog that declares a stage writing a
        // property the property_catalog doesn't know. The startup
        // self-check must trip before the catalogs return.
        let bad_stage_catalog = r#"
schema_version = 1

[bogus_stage]
input  = "source"
output = "drafts"
writes_properties = ["non_existent_property"]
"#;
        let err = Catalogs::load_from(PROPERTY_CATALOG_TOML, QUALITY_FLAGS_TOML, bad_stage_catalog)
            .unwrap_err();
        match err {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("non_existent_property") && msg.contains("property_catalog.toml"),
                    "self-check must name the bad key and the catalog: {msg}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }
}
