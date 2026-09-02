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
//!   start mid-line with a CJK reading. An anchor with nothing ahead
//!   of its first CJK character is not cut: the whole anchor stays the
//!   headword and the entry is stamped
//!   [`ANCHOR_WITHOUT_LATIN_HEAD_FLAG`], so the key it derives is
//!   non-empty and distinct from every other such entry in the book.
//! * [`split_headline_only`] — promote the anchor whole as the
//!   headword and join the body lines unchanged. The
//!   no-special-handling splitter used by bilingual entries that the
//!   earlier `pair_bilingual_entries` stage has already shaped.

use serde_json::Map;

use crate::core::{Ctx, RawEntry, SplitEntry, StageData};
use crate::error::ParseError;
use crate::pipeline::Stage;

/// Quality flag stamped by [`split_at_first_cjk`] on an entry whose
/// anchor has no latin text ahead of its first CJK character. Declared
/// in `crates/distill/data/quality_flags.toml`.
pub const ANCHOR_WITHOUT_LATIN_HEAD_FLAG: &str = "anchor_without_latin_head";

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

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let raws = data.expect_raws(self.name())?;
        let splits: Vec<SplitEntry> = raws.into_iter().map(raw_to_split_at_first_cjk).collect();
        ctx.coverage.splits = splits.len();
        Ok(StageData::Splits(splits))
    }
}

impl Stage for SplitHeadlineOnly {
    fn name(&self) -> &str {
        "split_headline_only"
    }

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let raws = data.expect_raws(self.name())?;
        let splits: Vec<SplitEntry> = raws.into_iter().map(raw_to_split_headline_only).collect();
        ctx.coverage.splits = splits.len();
        Ok(StageData::Splits(splits))
    }
}

fn raw_to_split_at_first_cjk(raw: RawEntry) -> SplitEntry {
    let mut anchor = raw.anchor.clone();
    let mut body_lines = raw.body.clone();
    let mut quality_flags = raw.quality_flags;

    if let Some(idx) = first_cjk_byte_index(&anchor) {
        let head = anchor[..idx].trim().to_string();
        let tail = anchor[idx..].trim().to_string();
        if head.is_empty() {
            // No latin head to cut off: keep the anchor whole rather
            // than emit an empty headword that would collide with
            // every other headless entry on `(book_slug, entry_key)`.
            anchor = tail;
            quality_flags.push(ANCHOR_WITHOUT_LATIN_HEAD_FLAG.to_string());
        } else {
            if !tail.is_empty() {
                body_lines.insert(0, tail);
            }
            anchor = head;
        }
    }

    SplitEntry {
        page: raw.page,
        sheet: raw.sheet,
        headword: anchor,
        body: join_body(&body_lines),
        lang: raw.lang,
        payload: Map::new(),
        quality_flags,
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
        quality_flags: raw.quality_flags,
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
            quality_flags: vec![],
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
    fn both_splitters_record_the_split_count_in_coverage() {
        for stage in [split_at_first_cjk(), split_headline_only()] {
            let name = stage.name().to_string();
            let inputs = vec![raw("Smith", vec!["one"]), raw("Jones", vec!["two"])];
            let mut ctx = Ctx::new();
            let out = stage.run(StageData::Raws(inputs), &mut ctx).expect("run");
            assert!(matches!(out, StageData::Splits(ref s) if s.len() == 2));
            assert_eq!(
                ctx.coverage.splits, 2,
                "{name} must record its split count in coverage",
            );
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
    fn split_at_first_cjk_keeps_an_anchor_that_opens_with_cjk_as_the_headword() {
        let inputs = vec![raw(" \u{53F2}\u{5BC6}\u{65AF}", vec!["some body"])];
        let out = run(split_at_first_cjk(), inputs);
        assert_eq!(
            out[0].headword, "\u{53F2}\u{5BC6}\u{65AF}",
            "an anchor with no latin head keeps its whole text as the headword",
        );
        assert_eq!(out[0].body, "some body", "nothing moves into the body");
        assert!(
            out[0]
                .quality_flags
                .iter()
                .any(|f| f == ANCHOR_WITHOUT_LATIN_HEAD_FLAG),
            "the entry must carry the flag: {:?}",
            out[0].quality_flags
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
