// SPDX-License-Identifier: Apache-2.0

//! Dispatch one `book.toml` stage reference to its matching builtin
//! constructor.
//!
//! Phase 7 sits between the catalog validator (which has already
//! confirmed that the stage name exists in `stage_catalog.toml` and
//! that the required params are present) and the runtime stage
//! constructors. The single entry point [`dispatch_stage`] is a big
//! match over the catalog-declared name; per-stage parameter decoding
//! lives in the named helpers below.
//!
//! Adding a new builtin is a three-place edit: a stage_catalog entry,
//! a Rust constructor under one of the segment / splitter / extractor
//! / finalize modules, and a new arm here. The `@script::` and
//! `@llm::` escape hatches never reach this point; they are caught
//! upstream by [`crate::catalogs::Catalogs::validate_book`].

use std::collections::BTreeMap;

use toml::Value as TomlValue;

use crate::anchors::{AnchorRule, LangAnchorRule};
use crate::error::ParseError;
use crate::extractor::{
    SearchTarget, extract_bracketed_tag, extract_gender_tag, extract_quotes, extract_year_span,
    partition_body_around_match, split_variants, unpack_paired_body,
};
use crate::finalize::{KeyNormalizer, to_entry_draft};
use crate::patterns::{BracketKind, PatternRef};
use crate::pipeline::Stage;
use crate::segment::{
    one_block_per_page, pair_bilingual_entries, split_bilingual_blocks, split_pages, walk_anchors,
    walk_anchors_per_lang,
};
use crate::splitter::{split_at_first_cjk, split_headline_only};

/// Build the runtime stage matching `name` and bind it to the
/// parameter map from the book.toml entry.
pub fn dispatch_stage(
    name: &str,
    params: Option<&BTreeMap<String, TomlValue>>,
) -> Result<Box<dyn Stage>, ParseError> {
    match name {
        // ---- segment ----
        "split_pages" => Ok(split_pages()),
        "one_block_per_page" => {
            let lang = get_string(params, "lang", false)?;
            Ok(one_block_per_page(lang))
        }
        "split_bilingual_blocks" => Ok(split_bilingual_blocks()),
        "walk_anchors" => {
            let anchor = decode_anchor_rule(require_value(params, "anchor")?)?;
            let reject = optional_value(params, "reject")
                .map(decode_anchor_rules)
                .transpose()?
                .unwrap_or_default();
            let drop_lone = get_bool(params, "drop_lone_letter_dividers", false)?.unwrap_or(false);
            let splice = get_bool(params, "splice_orphans_to_prev_block", false)?.unwrap_or(true);
            Ok(walk_anchors(anchor, reject, drop_lone, splice))
        }
        "walk_anchors_per_lang" => {
            let rules_val = require_value(params, "rules")?;
            let rules = rules_val
                .as_array()
                .ok_or_else(|| violation("walk_anchors_per_lang.rules must be an array"))?
                .iter()
                .map(decode_lang_anchor_rule)
                .collect::<Result<_, _>>()?;
            Ok(walk_anchors_per_lang(rules))
        }
        "pair_bilingual_entries" => {
            let primary = require_string(params, "primary_lang")?;
            let secondary = require_string(params, "secondary_lang")?;
            let merge = require_string(params, "merge_key")?;
            Ok(pair_bilingual_entries(primary, secondary, merge))
        }

        // ---- splitter ----
        "split_at_first_cjk" => Ok(split_at_first_cjk()),
        "split_headline_only" => Ok(split_headline_only()),

        // ---- extractor ----
        "extract_year_span" => {
            let key = require_string(params, "payload_key")?;
            Ok(extract_year_span(key))
        }
        "extract_bracketed_tag" => {
            let brackets = decode_bracket_kinds(require_value(params, "brackets")?)?;
            let key = require_string(params, "payload_key")?;
            let target = optional_value(params, "search_in")
                .map(decode_search_target)
                .transpose()?
                .unwrap_or(SearchTarget::Headword);
            Ok(extract_bracketed_tag(brackets, key, target))
        }
        "extract_gender_tag" => {
            let key = require_string(params, "payload_key")?;
            Ok(extract_gender_tag(key))
        }
        "split_variants" => {
            // `payload_key` is optional; the catalog default is `variants`.
            let key =
                get_string(params, "payload_key", false)?.unwrap_or_else(|| "variants".to_string());
            let sep = require_string(params, "sep")?;
            split_variants(key, sep)
        }
        "extract_quotes" => {
            let key = require_string(params, "payload_key")?;
            Ok(extract_quotes(key))
        }
        "partition_body_around_match" => {
            let pattern = decode_pattern_ref(require_value(params, "pattern")?)?;
            let head_key = require_string(params, "head_payload_key")?;
            let head_split_by = get_string(params, "head_split_by", false)?;
            let first_to = get_string(params, "first_to", false)?;
            let rest_to = get_string(params, "rest_to", false)?;
            let tail_to = get_string(params, "tail_to", false)?;
            partition_body_around_match(
                pattern,
                head_key,
                head_split_by,
                first_to,
                rest_to,
                tail_to,
            )
        }
        "unpack_paired_body" => {
            let hm = require_string(params, "head_marker")?;
            let bm = require_string(params, "body_marker")?;
            let ht = require_string(params, "head_to")?;
            let bt = require_string(params, "body_to")?;
            let ot = require_string(params, "original_to")?;
            Ok(unpack_paired_body(hm, bm, ht, bt, ot))
        }

        // ---- finalize ----
        "to_entry_draft" => {
            let normalizer = decode_key_normalizer(require_value(params, "key_normalizer")?)?;
            let carry = get_string_array(params, "carry_payload_keys")?.unwrap_or_default();
            let aliases = get_string_array(params, "aliases_from_payload")?.unwrap_or_default();
            Ok(to_entry_draft(normalizer, carry, aliases, None))
        }

        // ---- unknown ----
        other => Err(ParseError::StageNotFound(other.to_string())),
    }
}

// --- parameter accessors ----------------------------------------------------

fn require_value<'a>(
    params: Option<&'a BTreeMap<String, TomlValue>>,
    key: &str,
) -> Result<&'a TomlValue, ParseError> {
    params
        .and_then(|p| p.get(key))
        .ok_or_else(|| violation(format!("missing required param {key:?}")))
}

fn optional_value<'a>(
    params: Option<&'a BTreeMap<String, TomlValue>>,
    key: &str,
) -> Option<&'a TomlValue> {
    params.and_then(|p| p.get(key))
}

fn require_string(
    params: Option<&BTreeMap<String, TomlValue>>,
    key: &str,
) -> Result<String, ParseError> {
    let value = require_value(params, key)?;
    value
        .as_str()
        .map(String::from)
        .ok_or_else(|| violation(format!("param {key:?} must be a string")))
}

fn get_string(
    params: Option<&BTreeMap<String, TomlValue>>,
    key: &str,
    required: bool,
) -> Result<Option<String>, ParseError> {
    match optional_value(params, key) {
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| violation(format!("param {key:?} must be a string"))),
        None => {
            if required {
                Err(violation(format!("missing required param {key:?}")))
            } else {
                Ok(None)
            }
        }
    }
}

fn get_bool(
    params: Option<&BTreeMap<String, TomlValue>>,
    key: &str,
    required: bool,
) -> Result<Option<bool>, ParseError> {
    match optional_value(params, key) {
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| violation(format!("param {key:?} must be a bool"))),
        None => {
            if required {
                Err(violation(format!("missing required param {key:?}")))
            } else {
                Ok(None)
            }
        }
    }
}

fn get_string_array(
    params: Option<&BTreeMap<String, TomlValue>>,
    key: &str,
) -> Result<Option<Vec<String>>, ParseError> {
    let Some(value) = optional_value(params, key) else {
        return Ok(None);
    };
    let arr = value
        .as_array()
        .ok_or_else(|| violation(format!("param {key:?} must be an array")))?;
    let mut out = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let s = item
            .as_str()
            .ok_or_else(|| violation(format!("param {key:?}[{idx}] must be a string")))?;
        out.push(s.to_string());
    }
    Ok(Some(out))
}

// --- value decoders --------------------------------------------------------

fn decode_anchor_rule(value: &TomlValue) -> Result<AnchorRule, ParseError> {
    if let Some(s) = value.as_str() {
        return match s {
            "latin_headword" => Ok(AnchorRule::LatinHeadword),
            "running_header" => Ok(AnchorRule::RejectRunningHeader),
            other => Err(violation(format!(
                "unknown anchor rule {other:?}; expected latin_headword | running_header \
                 or a table form (cjk_short_headword / english_short_headword / any_of)"
            ))),
        };
    }
    if let Some(tbl) = value.as_table() {
        if let Some(inner) = tbl.get("cjk_short_headword") {
            let raw = inner
                .get("max_chars")
                .and_then(TomlValue::as_integer)
                .ok_or_else(|| violation("cjk_short_headword needs a max_chars integer"))?;
            let max_chars = usize::try_from(raw).map_err(|_| {
                violation(format!(
                    "cjk_short_headword.max_chars must be a non-negative integer, got {raw}"
                ))
            })?;
            return Ok(AnchorRule::CjkShortHeadword { max_chars });
        }
        if let Some(inner) = tbl.get("english_short_headword") {
            let raw = inner
                .get("max_words")
                .and_then(TomlValue::as_integer)
                .ok_or_else(|| violation("english_short_headword needs a max_words integer"))?;
            let max_words = usize::try_from(raw).map_err(|_| {
                violation(format!(
                    "english_short_headword.max_words must be a non-negative integer, got {raw}"
                ))
            })?;
            return Ok(AnchorRule::EnglishShortHeadword { max_words });
        }
        if let Some(inner) = tbl.get("any_of") {
            let arr = inner
                .as_array()
                .ok_or_else(|| violation("any_of value must be an array of anchor rules"))?;
            let rules = arr
                .iter()
                .map(decode_anchor_rule)
                .collect::<Result<_, _>>()?;
            return Ok(AnchorRule::AnyOf(rules));
        }
    }
    Err(violation(format!(
        "could not decode anchor rule from {value:?}"
    )))
}

fn decode_anchor_rules(value: &TomlValue) -> Result<Vec<AnchorRule>, ParseError> {
    let arr = value
        .as_array()
        .ok_or_else(|| violation("anchor rule list must be an array"))?;
    arr.iter().map(decode_anchor_rule).collect()
}

fn decode_bracket_kind(value: &TomlValue) -> Result<BracketKind, ParseError> {
    let s = value
        .as_str()
        .ok_or_else(|| violation("bracket kind must be a string"))?;
    match s {
        "angle" => Ok(BracketKind::Angle),
        "square" => Ok(BracketKind::Square),
        "paren" => Ok(BracketKind::Paren),
        other => Err(violation(format!(
            "unknown bracket kind {other:?}; expected angle | square | paren"
        ))),
    }
}

/// Accept either a single string (`"square"`) or an array of strings
/// (`["angle", "square"]`) and normalize to `Vec<BracketKind>`.
fn decode_bracket_kinds(value: &TomlValue) -> Result<Vec<BracketKind>, ParseError> {
    if value.as_str().is_some() {
        return decode_bracket_kind(value).map(|b| vec![b]);
    }
    let arr = value
        .as_array()
        .ok_or_else(|| violation("brackets must be a string or array of strings"))?;
    arr.iter().map(decode_bracket_kind).collect()
}

fn decode_pattern_ref(value: &TomlValue) -> Result<PatternRef, ParseError> {
    let tbl = value
        .as_table()
        .ok_or_else(|| violation("pattern must be a table"))?;
    if let Some(brackets_val) = tbl.get("bracketed_tag") {
        let brackets = decode_bracket_kinds(brackets_val)?;
        return Ok(PatternRef::BracketedTag { brackets });
    }
    if let Some(regex_val) = tbl.get("regex") {
        let s = regex_val
            .as_str()
            .ok_or_else(|| violation("regex pattern must be a string"))?;
        // Compile here so a syntactically broken pattern surfaces at
        // book.toml load time instead of being silently demoted to
        // "no match" by the `Regex::new(...).ok()?` at the stage call
        // site.
        if let Err(err) = regex::Regex::new(s) {
            return Err(ParseError::InvalidPattern {
                pattern: s.to_string(),
                reason: err.to_string(),
            });
        }
        return Ok(PatternRef::Regex(s.to_string()));
    }
    Err(violation(
        "pattern must contain `bracketed_tag` or `regex` keys",
    ))
}

fn decode_search_target(value: &TomlValue) -> Result<SearchTarget, ParseError> {
    let s = value
        .as_str()
        .ok_or_else(|| violation("search_in must be a string"))?;
    match s {
        "body" => Ok(SearchTarget::Body),
        "headword" => Ok(SearchTarget::Headword),
        other => Err(violation(format!(
            "unknown search_in {other:?}; expected body | headword"
        ))),
    }
}

fn decode_key_normalizer(value: &TomlValue) -> Result<KeyNormalizer, ParseError> {
    let s = value
        .as_str()
        .ok_or_else(|| violation("key_normalizer must be a string"))?;
    match s {
        "normalize_latin_key" => Ok(KeyNormalizer::NormalizeLatinKey),
        "normalize_cjk_key" => Ok(KeyNormalizer::NormalizeCjkKey),
        other => Err(violation(format!(
            "unknown key_normalizer {other:?}; \
             expected normalize_latin_key | normalize_cjk_key"
        ))),
    }
}

fn decode_lang_anchor_rule(value: &TomlValue) -> Result<LangAnchorRule, ParseError> {
    let tbl = value
        .as_table()
        .ok_or_else(|| violation("lang anchor rule entry must be a table"))?;
    let lang = tbl
        .get("lang")
        .and_then(TomlValue::as_str)
        .ok_or_else(|| violation("lang anchor rule needs a `lang` string"))?
        .to_string();
    let anchor = decode_anchor_rule(
        tbl.get("anchor")
            .ok_or_else(|| violation("lang anchor rule needs an `anchor`"))?,
    )?;
    let reject = match tbl.get("reject") {
        Some(v) => decode_anchor_rules(v)?,
        None => Vec::new(),
    };
    let drop_lone = tbl
        .get("drop_lone_letter_dividers")
        .and_then(TomlValue::as_bool)
        .unwrap_or(false);
    let splice = tbl
        .get("splice_orphans_to_prev_block")
        .and_then(TomlValue::as_bool)
        .unwrap_or(true);
    Ok(LangAnchorRule {
        lang,
        anchor,
        reject,
        drop_lone_letter_dividers: drop_lone,
        splice_orphans_to_prev_block: splice,
    })
}

fn violation<S: Into<String>>(msg: S) -> ParseError {
    ParseError::CatalogViolation(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor_table(toml_src: &str) -> TomlValue {
        let table: toml::Table = toml_src.parse().expect("test toml parses");
        TomlValue::Table(table)
    }

    #[test]
    fn cjk_short_headword_rejects_negative_max_chars() {
        let v = anchor_table("cjk_short_headword = { max_chars = -1 }");
        let err = decode_anchor_rule(&v).expect_err("negative max_chars must error");
        match err {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("cjk_short_headword.max_chars") && msg.contains("-1"),
                    "message was {msg:?}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }

    #[test]
    fn english_short_headword_rejects_negative_max_words() {
        let v = anchor_table("english_short_headword = { max_words = -2 }");
        let err = decode_anchor_rule(&v).expect_err("negative max_words must error");
        match err {
            ParseError::CatalogViolation(msg) => {
                assert!(
                    msg.contains("english_short_headword.max_words") && msg.contains("-2"),
                    "message was {msg:?}"
                );
            }
            other => panic!("expected CatalogViolation, got {other:?}"),
        }
    }

    /// A `regex` pattern that does not compile must fail the
    /// book.toml load with `InvalidPattern`, not slip through as a
    /// `PatternRef::Regex` that the stage call site would silently
    /// demote to "no match" via the historical `Regex::new(...).ok()?`.
    #[test]
    fn decode_pattern_ref_errors_on_a_broken_regex() {
        let mut tbl = toml::value::Table::new();
        tbl.insert(
            "regex".to_string(),
            TomlValue::String("(unclosed".to_string()),
        );
        let value = TomlValue::Table(tbl);
        let err = decode_pattern_ref(&value).expect_err("broken regex must error at load time");
        match err {
            ParseError::InvalidPattern { pattern, reason } => {
                assert_eq!(pattern, "(unclosed");
                assert!(!reason.is_empty(), "reason must carry compile diagnostic");
            }
            other => panic!("expected InvalidPattern, got {other:?}"),
        }
    }

    /// A well-formed `regex` still round-trips into a `PatternRef::Regex`.
    #[test]
    fn decode_pattern_ref_accepts_a_well_formed_regex() {
        let mut tbl = toml::value::Table::new();
        tbl.insert(
            "regex".to_string(),
            TomlValue::String(r"\b\d{4}\b".to_string()),
        );
        let value = TomlValue::Table(tbl);
        let pattern = decode_pattern_ref(&value).expect("ok");
        match pattern {
            PatternRef::Regex(s) => assert_eq!(s, r"\b\d{4}\b"),
            other => panic!("expected Regex, got {other:?}"),
        }
    }
}
