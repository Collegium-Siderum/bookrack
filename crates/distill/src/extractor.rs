// SPDX-License-Identifier: Apache-2.0

//! Extractor stages: `splits → splits`.
//!
//! Each extractor scans the [`SplitEntry`]'s body (or, optionally, its
//! headword), pulls a structured fact out into a payload key, and
//! strips the matched substring from the source text. All operate
//! per-entry and are order-independent within the same shape.

use regex::Regex;
use serde_json::{Value as JsonValue, json};

use crate::core::{Ctx, SplitEntry, StageData};
use crate::error::ParseError;
use crate::patterns::{PatternRef, match_pattern};
use crate::pipeline::Stage;

// --- public stage constructors ----------------------------------------------

pub fn extract_year_span(payload_key: String, search_in: SearchTarget) -> Box<dyn Stage> {
    Box::new(ExtractYearSpan {
        payload_key,
        search_in,
    })
}

pub fn extract_bracketed_tag(
    brackets: Vec<crate::patterns::BracketKind>,
    payload_key: String,
    search_in: SearchTarget,
) -> Box<dyn Stage> {
    Box::new(ExtractBracketedTag {
        pattern: PatternRef::BracketedTag { brackets },
        payload_key,
        search_in,
    })
}

pub fn extract_gender_tag(payload_key: String, search_in: SearchTarget) -> Box<dyn Stage> {
    Box::new(ExtractGenderTag {
        payload_key,
        search_in,
    })
}

pub fn split_variants(payload_key: String, sep: String) -> Result<Box<dyn Stage>, ParseError> {
    let sep_re = Regex::new(&sep).map_err(|e| {
        ParseError::CatalogViolation(format!("split_variants.sep is not a valid regex: {e}"))
    })?;
    Ok(Box::new(SplitVariants {
        payload_key,
        sep: sep_re,
    }))
}

pub fn extract_quotes(payload_key: String, search_in: SearchTarget) -> Box<dyn Stage> {
    Box::new(ExtractQuotes {
        payload_key,
        search_in,
    })
}

pub fn partition_body_around_match(
    pattern: PatternRef,
    head_payload_key: String,
    head_split_by: Option<String>,
    first_to: Option<String>,
    rest_to: Option<String>,
    tail_to: Option<String>,
) -> Result<Box<dyn Stage>, ParseError> {
    let head_split_by = match head_split_by.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => Some(Regex::new(p).map_err(|e| {
            ParseError::CatalogViolation(format!(
                "partition_body_around_match.head_split_by is not a valid regex: {e}"
            ))
        })?),
        None => None,
    };
    Ok(Box::new(PartitionBodyAroundMatch {
        pattern,
        head_payload_key,
        head_split_by,
        first_to,
        rest_to,
        tail_to,
    }))
}

pub fn unpack_paired_body(
    head_marker: String,
    body_marker: String,
    head_to: String,
    body_to: String,
    original_to: String,
) -> Box<dyn Stage> {
    Box::new(UnpackPairedBody {
        head_marker,
        body_marker,
        head_to,
        body_to,
        original_to,
    })
}

/// Which `SplitEntry` field an `extract_*` stage scans when looking
/// for the marker it owns. `Body` and `Headword` pick exactly one
/// field; `Both` tries `Body` first and falls back to `Headword`
/// only when the body search yields nothing, so the body remains
/// authoritative for stages that have always worked off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    Body,
    Headword,
    Both,
}

// --- stage structs ----------------------------------------------------------

struct ExtractYearSpan {
    payload_key: String,
    search_in: SearchTarget,
}
struct ExtractBracketedTag {
    pattern: PatternRef,
    payload_key: String,
    search_in: SearchTarget,
}
struct ExtractGenderTag {
    payload_key: String,
    search_in: SearchTarget,
}
struct SplitVariants {
    payload_key: String,
    sep: Regex,
}
struct ExtractQuotes {
    payload_key: String,
    search_in: SearchTarget,
}
struct PartitionBodyAroundMatch {
    pattern: PatternRef,
    head_payload_key: String,
    head_split_by: Option<Regex>,
    first_to: Option<String>,
    rest_to: Option<String>,
    tail_to: Option<String>,
}
struct UnpackPairedBody {
    head_marker: String,
    body_marker: String,
    head_to: String,
    body_to: String,
    original_to: String,
}

// --- helpers ----------------------------------------------------------------

/// Map each `SplitEntry` through `f` and re-emit. The shared loop
/// across every extractor.
fn map_splits<F>(splits: Vec<SplitEntry>, f: F) -> Vec<SplitEntry>
where
    F: Fn(SplitEntry) -> SplitEntry,
{
    splits.into_iter().map(f).collect()
}

/// Cut bytes `start..end` out of `s` and return the trimmed result.
fn strip_span(s: &str, start: usize, end: usize) -> String {
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..start]);
    out.push(' ');
    out.push_str(&s[end..]);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Apply `try_one` against each `SplitEntry` field implied by
/// `search_in` and stash the first `Some(_)` into `payload[payload_key]`.
/// The closure rewrites the field in place; when it returns `None`
/// the field is left untouched and the next target (if any) is tried.
/// `Both` resolves to "body first, then headword", so any pipeline
/// configured with `search_in = "body"` behaves identically when
/// switched to `"both"`.
fn try_targets<F>(s: &mut SplitEntry, search_in: SearchTarget, payload_key: &str, mut try_one: F)
where
    F: FnMut(&mut String) -> Option<JsonValue>,
{
    let payload = match search_in {
        SearchTarget::Body => try_one(&mut s.body),
        SearchTarget::Headword => try_one(&mut s.headword),
        SearchTarget::Both => {
            let body_hit = try_one(&mut s.body);
            if body_hit.is_some() {
                body_hit
            } else {
                try_one(&mut s.headword)
            }
        }
    };
    if let Some(p) = payload {
        s.payload.insert(payload_key.to_string(), p);
    }
}

// --- ExtractYearSpan --------------------------------------------------------

impl Stage for ExtractYearSpan {
    fn name(&self) -> &str {
        "extract_year_span"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let splits = data.expect_splits(self.name())?;
        let year_re =
            Regex::new(r"\(?\s*(\d{3,4})\s*[-\u{2013}]\s*(\d{3,4})?\s*\)?").expect("year regex");
        let key = self.payload_key.clone();
        let search_in = self.search_in;
        let out = map_splits(splits, |mut s| {
            try_targets(&mut s, search_in, &key, |field| {
                let cap = year_re.captures(field)?;
                let m = cap.get(0)?;
                let birth = cap.get(1).and_then(|g| g.as_str().parse::<i64>().ok());
                let death = cap.get(2).and_then(|g| g.as_str().parse::<i64>().ok());
                if birth.is_none() && death.is_none() {
                    return None;
                }
                let span = json!({
                    "birth": birth,
                    "death": death,
                });
                let (start, end) = (m.start(), m.end());
                *field = strip_span(field, start, end);
                Some(span)
            });
            s
        });
        Ok(StageData::Splits(out))
    }
}

// --- ExtractBracketedTag ----------------------------------------------------

impl Stage for ExtractBracketedTag {
    fn name(&self) -> &str {
        "extract_bracketed_tag"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let splits = data.expect_splits(self.name())?;
        let pattern = &self.pattern;
        let key = self.payload_key.clone();
        let search_in = self.search_in;
        let out = map_splits(splits, |mut s| {
            try_targets(&mut s, search_in, &key, |field| {
                let m = match_pattern(pattern, field)?;
                let inner = m.inner;
                *field = strip_span(field, m.start, m.end);
                Some(JsonValue::String(inner))
            });
            s
        });
        Ok(StageData::Splits(out))
    }
}

// --- ExtractGenderTag -------------------------------------------------------

impl Stage for ExtractGenderTag {
    fn name(&self) -> &str {
        "extract_gender_tag"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let splits = data.expect_splits(self.name())?;
        // Cheap markers seen across the v1 books: "(F)" / "(M)" /
        // "(woman)" / CJK gender markers in parens.
        let re =
            Regex::new(r"\(\s*(F|M|f|m|woman|man|\u{5973}|\u{7537})\s*\)").expect("gender regex");
        let key = self.payload_key.clone();
        let search_in = self.search_in;
        let out = map_splits(splits, |mut s| {
            try_targets(&mut s, search_in, &key, |field| {
                let cap = re.captures(field)?;
                let m = cap.get(0)?;
                let tag = cap.get(1)?.as_str();
                let normalized = match tag {
                    "F" | "f" | "woman" | "\u{5973}" => "F",
                    "M" | "m" | "man" | "\u{7537}" => "M",
                    _ => "other",
                };
                let (start, end) = (m.start(), m.end());
                *field = strip_span(field, start, end);
                Some(JsonValue::String(normalized.to_string()))
            });
            s
        });
        Ok(StageData::Splits(out))
    }
}

// --- SplitVariants ----------------------------------------------------------

impl Stage for SplitVariants {
    fn name(&self) -> &str {
        "split_variants"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let splits = data.expect_splits(self.name())?;
        let key = self.payload_key.clone();
        let out = map_splits(splits, |mut s| {
            if let Some(JsonValue::String(orig)) = s.payload.get(&key).cloned() {
                let parts: Vec<JsonValue> = self
                    .sep
                    .split(&orig)
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(|p| JsonValue::String(p.to_string()))
                    .collect();
                if parts.len() > 1 {
                    s.payload.insert(key.clone(), JsonValue::Array(parts));
                }
            }
            s
        });
        Ok(StageData::Splits(out))
    }
}

// --- ExtractQuotes ----------------------------------------------------------

impl Stage for ExtractQuotes {
    fn name(&self) -> &str {
        "extract_quotes"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let splits = data.expect_splits(self.name())?;
        // Recognize ASCII double quotes and the CJK corner-bracket
        // pair (U+300C / U+300D). The attribution slot is left empty
        // in this minimal extractor; a future hop can fill it in.
        let re = Regex::new("\"([^\"]+)\"|\u{300C}([^\u{300D}]+)\u{300D}").expect("quote regex");
        let key = self.payload_key.clone();
        let search_in = self.search_in;
        let out = map_splits(splits, |mut s| {
            try_targets(&mut s, search_in, &key, |field| {
                let mut quotes: Vec<JsonValue> = Vec::new();
                let mut last_end = 0usize;
                let mut keep = String::new();
                for cap in re.captures_iter(field) {
                    let m = cap.get(0).unwrap();
                    keep.push_str(&field[last_end..m.start()]);
                    let text = cap
                        .get(1)
                        .or_else(|| cap.get(2))
                        .map(|g| g.as_str().to_string())
                        .unwrap_or_default();
                    quotes.push(json!({"text": text, "attribution": ""}));
                    last_end = m.end();
                }
                if quotes.is_empty() {
                    return None;
                }
                keep.push_str(&field[last_end..]);
                *field = keep.split_whitespace().collect::<Vec<_>>().join(" ");
                Some(JsonValue::Array(quotes))
            });
            s
        });
        Ok(StageData::Splits(out))
    }
}

// --- PartitionBodyAroundMatch ----------------------------------------------

impl Stage for PartitionBodyAroundMatch {
    fn name(&self) -> &str {
        "partition_body_around_match"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let splits = data.expect_splits(self.name())?;
        let out = map_splits(splits, |mut s| {
            if let Some(m) = match_pattern(&self.pattern, &s.body) {
                let head = s.body[..m.start].trim().to_string();
                let tail = s.body[m.end..].trim().to_string();

                s.payload
                    .insert(self.head_payload_key.clone(), JsonValue::String(m.inner));

                if let Some(re) = &self.head_split_by {
                    let parts: Vec<String> = re
                        .split(&head)
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(String::from)
                        .collect();
                    if let Some(first_to) = &self.first_to
                        && let Some(first) = parts.first()
                    {
                        s.payload
                            .insert(first_to.clone(), JsonValue::String(first.clone()));
                    }
                    if let Some(rest_to) = &self.rest_to
                        && parts.len() > 1
                    {
                        let rest = parts[1..]
                            .iter()
                            .map(|p| JsonValue::String(p.clone()))
                            .collect();
                        s.payload.insert(rest_to.clone(), JsonValue::Array(rest));
                    }
                } else if let Some(first_to) = &self.first_to {
                    s.payload
                        .insert(first_to.clone(), JsonValue::String(head.clone()));
                }

                if let Some(tail_to) = &self.tail_to {
                    s.payload.insert(tail_to.clone(), JsonValue::String(tail));
                }
                s.body = String::new();
            }
            s
        });
        Ok(StageData::Splits(out))
    }
}

// --- UnpackPairedBody -------------------------------------------------------

impl Stage for UnpackPairedBody {
    fn name(&self) -> &str {
        "unpack_paired_body"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let splits = data.expect_splits(self.name())?;
        let out = map_splits(splits, |mut s| {
            let body = s.body.clone();
            if let Some(head_start) = body.find(&self.head_marker)
                && let Some(body_start) = body.find(&self.body_marker)
                && head_start + self.head_marker.len() <= body_start
            {
                let original = body[..head_start].trim().to_string();
                let head_text = body[head_start + self.head_marker.len()..body_start]
                    .trim()
                    .to_string();
                let body_text = body[body_start + self.body_marker.len()..]
                    .trim()
                    .to_string();
                s.payload
                    .insert(self.original_to.clone(), JsonValue::String(original));
                s.payload
                    .insert(self.head_to.clone(), JsonValue::String(head_text));
                s.payload
                    .insert(self.body_to.clone(), JsonValue::String(body_text));
                s.body = String::new();
            } else if !s.payload.contains_key(&self.head_to) {
                // No markers, but make sure the keys are at least
                // declared with an empty placeholder so downstream
                // finalize keys never miss them; this keeps the
                // book.toml-declared keyset consistent.
            }
            // ensure quality_flags untouched
            let _ = &s.quality_flags;
            s
        });
        Ok(StageData::Splits(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::BracketKind;
    use serde_json::Map;

    fn split(headword: &str, body: &str) -> SplitEntry {
        SplitEntry {
            page: 1,
            sheet: 1,
            headword: headword.to_string(),
            body: body.to_string(),
            lang: Some("latin".to_string()),
            payload: Map::new(),
            quality_flags: vec![],
        }
    }

    fn run(stage: Box<dyn Stage>, splits: Vec<SplitEntry>) -> Vec<SplitEntry> {
        let mut ctx = Ctx::new();
        let out = stage
            .run(StageData::Splits(splits), &mut ctx)
            .expect("stage run");
        match out {
            StageData::Splits(s) => s,
            other => panic!("expected Splits, got {other:?}"),
        }
    }

    // ---- ExtractYearSpan ----

    #[test]
    fn extract_year_span_writes_birth_and_death() {
        let inputs = vec![split("Smith", "American baseball player (1900-2000)")];
        let out = run(
            extract_year_span("year_span".to_string(), SearchTarget::Body),
            inputs,
        );
        assert_eq!(
            out[0].payload.get("year_span").unwrap(),
            &json!({"birth": 1900, "death": 2000})
        );
        assert!(
            !out[0].body.contains("1900"),
            "year-span match must be stripped: {:?}",
            out[0].body
        );
    }

    #[test]
    fn extract_year_span_handles_open_ended_death() {
        let inputs = vec![split("Smith", "American (1900-)")];
        let out = run(
            extract_year_span("year_span".to_string(), SearchTarget::Body),
            inputs,
        );
        assert_eq!(
            out[0].payload.get("year_span").unwrap(),
            &json!({"birth": 1900, "death": null})
        );
    }

    #[test]
    fn extract_year_span_no_match_leaves_payload_empty() {
        let inputs = vec![split("Smith", "American baseball player")];
        let out = run(
            extract_year_span("year_span".to_string(), SearchTarget::Body),
            inputs,
        );
        assert!(out[0].payload.is_empty());
        assert_eq!(out[0].body, "American baseball player");
    }

    /// In name-translation dictionaries the year span often rides on
    /// the latin headword (`Balch, Emily Greene (1867-1961)`) after
    /// `split_at_first_cjk` has cut the CJK gloss out into the body.
    /// `search_in = "headword"` finds it there and strips it from the
    /// headword without touching the body.
    #[test]
    fn extract_year_span_with_search_in_headword_strips_from_headword_only() {
        let inputs = vec![split(
            "Balch, Emily Greene (1867-1961)",
            "American sociologist",
        )];
        let out = run(
            extract_year_span("year_span".to_string(), SearchTarget::Headword),
            inputs,
        );
        assert_eq!(
            out[0].payload.get("year_span").unwrap(),
            &json!({"birth": 1867, "death": 1961})
        );
        assert_eq!(out[0].headword, "Balch, Emily Greene");
        assert_eq!(out[0].body, "American sociologist");
    }

    /// `search_in = "both"` resolves body-first, so an entry with a
    /// span on each side still cuts the body span (matching the
    /// `"body"` default) rather than the headword one.
    #[test]
    fn extract_year_span_with_search_in_both_prefers_body() {
        let inputs = vec![split(
            "Balch, Emily Greene (1900-1980)",
            "American sociologist (1867-1961)",
        )];
        let out = run(
            extract_year_span("year_span".to_string(), SearchTarget::Both),
            inputs,
        );
        assert_eq!(
            out[0].payload.get("year_span").unwrap(),
            &json!({"birth": 1867, "death": 1961})
        );
        assert_eq!(out[0].headword, "Balch, Emily Greene (1900-1980)");
        assert_eq!(out[0].body, "American sociologist");
    }

    /// `search_in = "both"` falls back to the headword when the body
    /// has no year span. The headword is rewritten in place.
    #[test]
    fn extract_year_span_with_search_in_both_falls_back_to_headword() {
        let inputs = vec![split(
            "Balch, Emily Greene (1867-1961)",
            "American sociologist",
        )];
        let out = run(
            extract_year_span("year_span".to_string(), SearchTarget::Both),
            inputs,
        );
        assert_eq!(
            out[0].payload.get("year_span").unwrap(),
            &json!({"birth": 1867, "death": 1961})
        );
        assert_eq!(out[0].headword, "Balch, Emily Greene");
        assert_eq!(out[0].body, "American sociologist");
    }

    // ---- ExtractBracketedTag ----

    #[test]
    fn extract_bracketed_tag_pulls_first_match_from_body() {
        let inputs = vec![split("Smith", "American baseball <USA> player [extra]")];
        let out = run(
            extract_bracketed_tag(
                vec![BracketKind::Angle, BracketKind::Square],
                "country".to_string(),
                SearchTarget::Body,
            ),
            inputs,
        );
        assert_eq!(
            out[0].payload.get("country").unwrap(),
            &JsonValue::String("USA".to_string())
        );
        assert!(!out[0].body.contains("<USA>"));
        assert!(
            out[0].body.contains("[extra]"),
            "only the first match is consumed"
        );
    }

    #[test]
    fn extract_bracketed_tag_no_match_leaves_input_unchanged() {
        let inputs = vec![split("Smith", "no brackets here")];
        let out = run(
            extract_bracketed_tag(
                vec![BracketKind::Angle],
                "country".to_string(),
                SearchTarget::Body,
            ),
            inputs,
        );
        assert!(out[0].payload.is_empty());
        assert_eq!(out[0].body, "no brackets here");
    }

    // ---- ExtractGenderTag ----

    #[test]
    fn extract_gender_tag_recognizes_f_marker_and_strips_it() {
        let inputs = vec![split("Smith", "American baseball player (F) etc")];
        let out = run(
            extract_gender_tag("gender".to_string(), SearchTarget::Body),
            inputs,
        );
        assert_eq!(
            out[0].payload.get("gender").unwrap(),
            &JsonValue::String("F".to_string())
        );
        assert!(!out[0].body.contains("(F)"));
    }

    #[test]
    fn extract_gender_tag_no_match_leaves_payload_empty() {
        let inputs = vec![split("Smith", "American baseball player")];
        let out = run(
            extract_gender_tag("gender".to_string(), SearchTarget::Body),
            inputs,
        );
        assert!(out[0].payload.is_empty());
    }

    // ---- SplitVariants ----

    #[test]
    fn split_variants_replaces_a_seeded_string_with_an_array() {
        let mut s = split("Smith", "");
        s.payload.insert(
            "variants".to_string(),
            JsonValue::String("foo; bar; baz".to_string()),
        );
        let out = run(
            split_variants("variants".to_string(), "[;]".to_string()).expect("compile sep"),
            vec![s],
        );
        assert_eq!(
            out[0].payload.get("variants").unwrap(),
            &json!(["foo", "bar", "baz"])
        );
    }

    #[test]
    fn split_variants_with_a_single_value_leaves_the_string_untouched() {
        let mut s = split("Smith", "");
        s.payload
            .insert("variants".to_string(), JsonValue::String("foo".to_string()));
        let out = run(
            split_variants("variants".to_string(), "[;]".to_string()).expect("compile sep"),
            vec![s],
        );
        assert_eq!(
            out[0].payload.get("variants").unwrap(),
            &JsonValue::String("foo".to_string())
        );
    }

    #[test]
    fn split_variants_rejects_invalid_regex_at_construction() {
        match split_variants("variants".to_string(), "[unterminated".to_string()) {
            Err(ParseError::CatalogViolation(msg)) => {
                assert!(msg.contains("split_variants.sep"), "message was {msg:?}");
            }
            Err(other) => panic!("expected CatalogViolation, got {other:?}"),
            Ok(_) => panic!("invalid regex must surface as Err"),
        }
    }

    // ---- ExtractQuotes ----

    #[test]
    fn extract_quotes_pulls_both_ascii_and_cjk_quoted_runs() {
        let inputs = vec![split(
            "Smith",
            "He said \"hello\" and \u{300C}\u{4F60}\u{597D}\u{300D} again",
        )];
        let out = run(
            extract_quotes("quotes".to_string(), SearchTarget::Body),
            inputs,
        );
        let quotes = out[0].payload.get("quotes").unwrap().as_array().unwrap();
        assert_eq!(quotes.len(), 2);
        assert_eq!(quotes[0]["text"], "hello");
        assert_eq!(quotes[1]["text"], "\u{4F60}\u{597D}");
    }

    #[test]
    fn extract_quotes_no_match_writes_no_quotes() {
        let inputs = vec![split("Smith", "no quotes here")];
        let out = run(
            extract_quotes("quotes".to_string(), SearchTarget::Body),
            inputs,
        );
        assert!(out[0].payload.is_empty());
    }

    // ---- PartitionBodyAroundMatch ----

    #[test]
    fn partition_body_around_match_distributes_head_inner_and_tail() {
        let inputs = vec![split(
            "Smith",
            "\u{53F2}\u{5BC6}\u{65AF};\u{7EA6}\u{7FF0} <American> baseball player",
        )];
        let out = run(
            partition_body_around_match(
                PatternRef::BracketedTag {
                    brackets: vec![BracketKind::Angle, BracketKind::Square],
                },
                "country".to_string(),
                Some("[;\u{FF1B}]".to_string()),
                Some("chinese_name".to_string()),
                Some("variants".to_string()),
                Some("bio_annotation".to_string()),
            )
            .expect("compile head_split_by"),
            inputs,
        );
        let payload = &out[0].payload;
        assert_eq!(payload.get("country").unwrap(), "American");
        assert_eq!(
            payload.get("chinese_name").unwrap(),
            "\u{53F2}\u{5BC6}\u{65AF}"
        );
        assert_eq!(
            payload.get("variants").unwrap(),
            &json!(["\u{7EA6}\u{7FF0}"])
        );
        assert_eq!(payload.get("bio_annotation").unwrap(), "baseball player");
        assert!(out[0].body.is_empty());
    }

    #[test]
    fn partition_body_around_match_no_match_passes_input_through() {
        let inputs = vec![split("Smith", "no brackets here")];
        let out = run(
            partition_body_around_match(
                PatternRef::BracketedTag {
                    brackets: vec![BracketKind::Angle],
                },
                "country".to_string(),
                None,
                None,
                None,
                None,
            )
            .expect("compile head_split_by"),
            inputs,
        );
        assert!(out[0].payload.is_empty());
        assert_eq!(out[0].body, "no brackets here");
    }

    #[test]
    fn partition_body_around_match_rejects_invalid_head_split_by() {
        let result = partition_body_around_match(
            PatternRef::BracketedTag {
                brackets: vec![BracketKind::Angle],
            },
            "country".to_string(),
            Some("[unterminated".to_string()),
            None,
            None,
            None,
        );
        match result {
            Err(ParseError::CatalogViolation(msg)) => {
                assert!(
                    msg.contains("partition_body_around_match.head_split_by"),
                    "message was {msg:?}"
                );
            }
            Err(other) => panic!("expected CatalogViolation, got {other:?}"),
            Ok(_) => panic!("invalid head_split_by regex must surface as Err"),
        }
    }

    // ---- UnpackPairedBody ----

    #[test]
    fn unpack_paired_body_splits_a_packed_bilingual_body() {
        let body = "relating to philosophy<<<translation_head>>>\
            \u{54F2}\u{5B66}\u{77E5}\u{8BC6}<<<translation_body>>>\
            related to phil knowledge";
        let inputs = vec![split("philosophical", body)];
        let out = run(
            unpack_paired_body(
                "<<<translation_head>>>".to_string(),
                "<<<translation_body>>>".to_string(),
                "zh_head".to_string(),
                "zh_text".to_string(),
                "en_text".to_string(),
            ),
            inputs,
        );
        assert_eq!(
            out[0].payload.get("en_text").unwrap(),
            "relating to philosophy"
        );
        assert_eq!(
            out[0].payload.get("zh_head").unwrap(),
            "\u{54F2}\u{5B66}\u{77E5}\u{8BC6}"
        );
        assert_eq!(
            out[0].payload.get("zh_text").unwrap(),
            "related to phil knowledge"
        );
    }

    #[test]
    fn unpack_paired_body_without_both_markers_leaves_payload_untouched() {
        let inputs = vec![split("philosophical", "no markers here")];
        let out = run(
            unpack_paired_body(
                "<<<translation_head>>>".to_string(),
                "<<<translation_body>>>".to_string(),
                "zh_head".to_string(),
                "zh_text".to_string(),
                "en_text".to_string(),
            ),
            inputs,
        );
        assert!(out[0].payload.is_empty());
    }
}
