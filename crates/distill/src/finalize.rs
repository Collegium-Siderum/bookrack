// SPDX-License-Identifier: Apache-2.0

//! Finalize stage: `splits → drafts`.
//!
//! [`to_entry_draft`] turns each `SplitEntry` into the
//! `Refs::upsert_entry`-ready `EntryDraft`: normalizes the lookup
//! key, copies the declared `carry_payload_keys` forward, composes
//! `aliases` from the configured payload keys, and synthesises the
//! `source_json` block from `book_slug` / `distill_run_id` /
//! `ocr_engine` stashed in [`Ctx::extras`] by the orchestrator.

use serde_json::{Map, Value as JsonValue, json};

use crate::core::{Ctx, EntryDraft, SplitEntry, StageData};
use crate::error::ParseError;
use crate::pipeline::Stage;

/// Boxed closure shape for [`FtsComposer::Custom`].
pub type FtsComposerFn = Box<dyn Fn(&SplitEntry, &[String]) -> String + Send + Sync>;

/// Composer for the FTS5 text column. The `Default` variant
/// concatenates the headword, every alias, and every carried
/// payload value into one space-separated string. `Custom` hands
/// the question off to a caller-provided closure.
pub enum FtsComposer {
    Default,
    Custom(FtsComposerFn),
}

impl std::fmt::Debug for FtsComposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FtsComposer::Default => f.write_str("Default"),
            FtsComposer::Custom(_) => f.write_str("Custom(<closure>)"),
        }
    }
}

/// Normalization strategy for the lookup key. The mother doc § 5.5
/// note ties the lookup key form to the script of the headword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyNormalizer {
    /// Lower-case and strip non-alphanumeric characters; suited for
    /// latin-script headwords where minor punctuation drifts.
    NormalizeLatinKey,
    /// Trim outer whitespace only; suited for CJK headwords whose
    /// every character is already significant.
    NormalizeCjkKey,
}

impl KeyNormalizer {
    /// Project a headword into its stable lookup key form.
    pub fn normalize(self, headword: &str) -> String {
        match self {
            KeyNormalizer::NormalizeLatinKey => headword
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect(),
            KeyNormalizer::NormalizeCjkKey => headword.trim().to_string(),
        }
    }
}

/// Construct the finalize stage.
pub fn to_entry_draft(
    key_normalizer: KeyNormalizer,
    carry_payload_keys: Vec<String>,
    aliases_from_payload: Vec<String>,
    fts_composer: Option<FtsComposer>,
) -> Box<dyn Stage> {
    Box::new(ToEntryDraft {
        key_normalizer,
        carry_payload_keys,
        aliases_from_payload,
        fts_composer: fts_composer.unwrap_or(FtsComposer::Default),
    })
}

struct ToEntryDraft {
    key_normalizer: KeyNormalizer,
    carry_payload_keys: Vec<String>,
    aliases_from_payload: Vec<String>,
    fts_composer: FtsComposer,
}

impl Stage for ToEntryDraft {
    fn name(&self) -> &str {
        "to_entry_draft"
    }

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let splits = data.expect_splits(self.name())?;

        let book_slug = ctx
            .extras
            .get("book_slug")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string();
        let distill_run_id = ctx.extras.get("distill_run_id").cloned();
        let ocr_engine = ctx.extras.get("ocr_engine").cloned();

        let drafts = splits
            .into_iter()
            .map(|s| self.one_draft(s, &book_slug, &distill_run_id, &ocr_engine))
            .collect();

        Ok(StageData::Drafts(drafts))
    }
}

impl ToEntryDraft {
    fn one_draft(
        &self,
        split: SplitEntry,
        book_slug: &str,
        distill_run_id: &Option<JsonValue>,
        ocr_engine: &Option<JsonValue>,
    ) -> EntryDraft {
        let entry_key = self.key_normalizer.normalize(&split.headword);

        let mut payload = Map::new();
        for key in &self.carry_payload_keys {
            if let Some(v) = split.payload.get(key) {
                payload.insert(key.clone(), v.clone());
            }
        }

        let aliases = self.aliases_from(&split.payload);

        let fts_text = match &self.fts_composer {
            FtsComposer::Default => compose_default(&split.headword, &aliases, &payload),
            FtsComposer::Custom(f) => f(&split, &aliases),
        };

        let source = json!({
            "book_slug": book_slug,
            "page": split.page,
            "sheet": split.sheet,
            "distill_run_id": distill_run_id.clone().unwrap_or(JsonValue::Null),
            "ocr_engine": ocr_engine.clone().unwrap_or(JsonValue::Null),
        });

        EntryDraft {
            book_slug: book_slug.to_string(),
            entry_key,
            headword: split.headword,
            aliases,
            payload,
            fts_text,
            source,
            quality_flags: split.quality_flags,
        }
    }

    fn aliases_from(&self, payload: &Map<String, JsonValue>) -> Vec<String> {
        let mut out = Vec::new();
        for key in &self.aliases_from_payload {
            match payload.get(key) {
                Some(JsonValue::String(s)) if !s.is_empty() => out.push(s.clone()),
                Some(JsonValue::Array(arr)) => {
                    for v in arr {
                        if let Some(s) = v.as_str()
                            && !s.is_empty()
                        {
                            out.push(s.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// Space-separated concatenation of headword + aliases + all carried
/// payload values (string-flavoured ones, recursively for arrays).
fn compose_default(headword: &str, aliases: &[String], payload: &Map<String, JsonValue>) -> String {
    let mut parts: Vec<String> = vec![headword.to_string()];
    parts.extend(aliases.iter().cloned());
    for value in payload.values() {
        push_string_leaves(value, &mut parts);
    }
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_string_leaves(value: &JsonValue, out: &mut Vec<String>) {
    match value {
        JsonValue::String(s) => out.push(s.clone()),
        JsonValue::Array(arr) => {
            for v in arr {
                push_string_leaves(v, out);
            }
        }
        JsonValue::Object(obj) => {
            for v in obj.values() {
                push_string_leaves(v, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn split_with_payload(headword: &str, payload: Map<String, JsonValue>) -> SplitEntry {
        SplitEntry {
            page: 7,
            sheet: 9,
            headword: headword.to_string(),
            body: String::new(),
            lang: Some("latin".to_string()),
            payload,
            quality_flags: vec!["spliced_from_orphan".to_string()],
        }
    }

    fn run_finalize(
        stage: Box<dyn Stage>,
        splits: Vec<SplitEntry>,
        ctx: &mut Ctx,
    ) -> Vec<EntryDraft> {
        let out = stage
            .run(StageData::Splits(splits), ctx)
            .expect("finalize run");
        match out {
            StageData::Drafts(d) => d,
            other => panic!("expected Drafts, got {other:?}"),
        }
    }

    #[test]
    fn normalize_latin_key_strips_punctuation_and_case() {
        let key = KeyNormalizer::NormalizeLatinKey.normalize("Smith, J.");
        assert_eq!(key, "smithj");
    }

    #[test]
    fn normalize_cjk_key_keeps_codepoints_and_trims_outer_space() {
        let key = KeyNormalizer::NormalizeCjkKey.normalize("  \u{54F2}\u{5B66}\u{77E5}\u{8BC6}  ");
        assert_eq!(key, "\u{54F2}\u{5B66}\u{77E5}\u{8BC6}");
    }

    #[test]
    fn default_fts_composer_joins_headword_aliases_and_carried_payload() {
        let mut payload = Map::new();
        payload.insert(
            "chinese_name".to_string(),
            JsonValue::String("\u{53F2}\u{5BC6}\u{65AF}".to_string()),
        );
        payload.insert("variants".to_string(), json!(["\u{7EA6}\u{7FF0}"]));
        payload.insert(
            "year_span".to_string(),
            json!({"birth": 1900, "death": 2000}),
        );
        let stage = to_entry_draft(
            KeyNormalizer::NormalizeLatinKey,
            vec![
                "chinese_name".to_string(),
                "variants".to_string(),
                "year_span".to_string(),
            ],
            vec!["chinese_name".to_string(), "variants".to_string()],
            None,
        );
        let mut ctx = Ctx::new();
        ctx.extras.insert(
            "book_slug".to_string(),
            JsonValue::String("name_translation_xinhua".to_string()),
        );
        let drafts = run_finalize(
            stage,
            vec![split_with_payload("Smith, J.", payload)],
            &mut ctx,
        );
        let draft = &drafts[0];

        assert_eq!(draft.book_slug, "name_translation_xinhua");
        assert_eq!(draft.entry_key, "smithj");
        assert!(
            draft.fts_text.contains("Smith, J.")
                && draft.fts_text.contains("\u{53F2}\u{5BC6}\u{65AF}")
                && draft.fts_text.contains("\u{7EA6}\u{7FF0}"),
            "default composer must concatenate headword + aliases + payload string leaves: {:?}",
            draft.fts_text
        );
        assert!(
            draft.payload.contains_key("year_span"),
            "carry_payload_keys must preserve structured values: {:?}",
            draft.payload
        );
        assert_eq!(
            draft.aliases,
            vec![
                "\u{53F2}\u{5BC6}\u{65AF}".to_string(),
                "\u{7EA6}\u{7FF0}".to_string(),
            ]
        );
        assert_eq!(draft.source["page"], 7);
        assert_eq!(draft.source["sheet"], 9);
        assert_eq!(draft.quality_flags, vec!["spliced_from_orphan".to_string()]);
    }

    #[test]
    fn custom_fts_composer_replaces_the_default_join() {
        let stage = to_entry_draft(
            KeyNormalizer::NormalizeLatinKey,
            vec![],
            vec![],
            Some(FtsComposer::Custom(Box::new(|s, _aliases| {
                format!("custom:{}", s.headword)
            }))),
        );
        let mut ctx = Ctx::new();
        let drafts = run_finalize(
            stage,
            vec![split_with_payload("Smith", Map::new())],
            &mut ctx,
        );
        assert_eq!(drafts[0].fts_text, "custom:Smith");
    }

    #[test]
    fn source_block_reads_distill_run_id_and_ocr_engine_from_ctx() {
        let stage = to_entry_draft(KeyNormalizer::NormalizeLatinKey, vec![], vec![], None);
        let mut ctx = Ctx::new();
        ctx.extras.insert(
            "book_slug".to_string(),
            JsonValue::String("name_translation_xinhua".to_string()),
        );
        ctx.extras.insert(
            "distill_run_id".to_string(),
            JsonValue::String("2026-06-25T10:23:00Z".to_string()),
        );
        ctx.extras.insert(
            "ocr_engine".to_string(),
            JsonValue::String("polyocr glm 2026-06-22".to_string()),
        );
        let drafts = run_finalize(
            stage,
            vec![split_with_payload("Smith", Map::new())],
            &mut ctx,
        );
        assert_eq!(drafts[0].source["distill_run_id"], "2026-06-25T10:23:00Z");
        assert_eq!(drafts[0].source["ocr_engine"], "polyocr glm 2026-06-22");
    }
}
