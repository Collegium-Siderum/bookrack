// SPDX-License-Identifier: Apache-2.0

//! Splitter stages: `raws → splits`.
//!
//! Each splitter consumes a [`crate::core::RawEntry`] vector and emits
//! a [`crate::core::SplitEntry`] vector. The two splitters land in
//! this file:
//!
//! * [`split_at_first_cjk`] — partition the anchor at the first CJK
//!   character; the latin prefix becomes the headword, the CJK
//!   suffix folds back into the body. Used by the name-translation
//!   books, whose anchors are bare latin headwords but whose bodies
//!   start mid-line with a CJK reading.
//! * [`split_headline_only`] — promote the anchor whole as the
//!   headword and join the body lines unchanged. The
//!   no-special-handling splitter used by bilingual entries that the
//!   earlier `pair_bilingual_entries` stage has already shaped.

use serde_json::Map;

use crate::core::{Ctx, RawEntry, SplitEntry, StageData};
use crate::error::ParseError;
use crate::pipeline::Stage;

/// Construct a [`split_at_first_cjk`] stage.
pub fn split_at_first_cjk() -> Box<dyn Stage> {
    Box::new(SplitAtFirstCjk)
}

/// Construct a [`split_headline_only`] stage.
pub fn split_headline_only() -> Box<dyn Stage> {
    Box::new(SplitHeadlineOnly)
}

struct SplitAtFirstCjk;
struct SplitHeadlineOnly;

impl Stage for SplitAtFirstCjk {
    fn name(&self) -> &str {
        "split_at_first_cjk"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let raws = data.expect_raws(self.name())?;
        let splits = raws.into_iter().map(raw_to_split_at_first_cjk).collect();
        Ok(StageData::Splits(splits))
    }
}

impl Stage for SplitHeadlineOnly {
    fn name(&self) -> &str {
        "split_headline_only"
    }

    fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let raws = data.expect_raws(self.name())?;
        let splits = raws.into_iter().map(raw_to_split_headline_only).collect();
        Ok(StageData::Splits(splits))
    }
}

fn raw_to_split_at_first_cjk(raw: RawEntry) -> SplitEntry {
    let mut anchor = raw.anchor.clone();
    let mut body_lines = raw.body.clone();

    if let Some(idx) = first_cjk_byte_index(&anchor) {
        let head = anchor[..idx].trim().to_string();
        let tail = anchor[idx..].trim().to_string();
        if !tail.is_empty() {
            body_lines.insert(0, tail);
        }
        anchor = head;
    }

    SplitEntry {
        page: raw.page,
        sheet: raw.sheet,
        headword: anchor,
        body: join_body(&body_lines),
        lang: raw.lang,
        payload: Map::new(),
        quality_flags: vec![],
    }
}

fn raw_to_split_headline_only(raw: RawEntry) -> SplitEntry {
    SplitEntry {
        page: raw.page,
        sheet: raw.sheet,
        headword: raw.anchor,
        body: join_body(&raw.body),
        lang: raw.lang,
        payload: Map::new(),
        quality_flags: vec![],
    }
}

fn join_body(lines: &[String]) -> String {
    lines.join(" ").trim().to_string()
}

/// Byte index of the first CJK character in `s`, or `None`.
pub(crate) fn first_cjk_byte_index(s: &str) -> Option<usize> {
    s.char_indices().find(|(_, c)| is_cjk(*c)).map(|(i, _)| i)
}

/// True for the unified ideograph block plus extensions A and the
/// compatibility block. The check is intentionally inclusive: false
/// positives on rare symbol ranges are preferable to a miss that
/// leaves a CJK syllable inside the latin headword.
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{F900}'..='\u{FAFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(anchor: &str, body: Vec<&str>) -> RawEntry {
        RawEntry {
            page: 1,
            sheet: 1,
            anchor: anchor.to_string(),
            body: body.into_iter().map(String::from).collect(),
            lang: Some("latin".to_string()),
        }
    }

    fn run(stage: Box<dyn Stage>, raws: Vec<RawEntry>) -> Vec<SplitEntry> {
        let mut ctx = Ctx::new();
        let out = stage.run(StageData::Raws(raws), &mut ctx).expect("run");
        match out {
            StageData::Splits(s) => s,
            other => panic!("expected Splits, got {other:?}"),
        }
    }

    #[test]
    fn split_at_first_cjk_partitions_a_mixed_anchor_and_keeps_body_intact() {
        let inputs = vec![raw(
            "Smith\u{53F2}\u{5BC6}\u{65AF}",
            vec!["American baseball player"],
        )];
        let out = run(split_at_first_cjk(), inputs);
        assert_eq!(out[0].headword, "Smith");
        assert!(
            out[0].body.contains("\u{53F2}\u{5BC6}\u{65AF}"),
            "CJK suffix must move into the body: {:?}",
            out[0].body
        );
        assert!(
            out[0].body.contains("American baseball player"),
            "original body line must persist: {:?}",
            out[0].body
        );
    }

    #[test]
    fn split_at_first_cjk_passes_through_a_pure_latin_anchor_unchanged() {
        let inputs = vec![raw("Jones", vec!["British poet"])];
        let out = run(split_at_first_cjk(), inputs);
        assert_eq!(out[0].headword, "Jones");
        assert_eq!(out[0].body, "British poet");
    }

    #[test]
    fn split_headline_only_keeps_the_anchor_and_joins_the_body() {
        let inputs = vec![raw("Smith", vec!["line one", "line two"])];
        let out = run(split_headline_only(), inputs);
        assert_eq!(out[0].headword, "Smith");
        assert_eq!(out[0].body, "line one line two");
    }
}
