// SPDX-License-Identifier: Apache-2.0

//! Segment stages: `source → pages → blocks → raws`.
//!
//! Phase 5 lands the front half of the distill builtin chain. Each
//! constructor returns a `Box<dyn Stage>` matched 1:1 by the
//! stage-catalog entry of the same name. See
//! `crates/distill/data/stage_catalog.toml` for the public parameter
//! schema and `crates/distill/src/extractor.rs` and `finalize.rs`
//! for the downstream `splits → drafts` chain.

use regex::Regex;

use crate::anchors::{AnchorRule, LangAnchorRule};
use crate::core::{Block, Ctx, Page, RawEntry, StageData};
use crate::error::ParseError;
use crate::pipeline::Stage;
use crate::splitter::is_cjk;

// --- public constructors ---------------------------------------------------

pub fn split_pages() -> Box<dyn Stage> {
    Box::new(SplitPages)
}

pub fn one_block_per_page(default_lang: Option<String>) -> Box<dyn Stage> {
    Box::new(OneBlockPerPage { default_lang })
}

pub fn split_bilingual_blocks() -> Box<dyn Stage> {
    Box::new(SplitBilingualBlocks)
}

pub fn walk_anchors(
    anchor: AnchorRule,
    reject: Vec<AnchorRule>,
    drop_lone_letter_dividers: bool,
    splice_orphans_to_prev_block: bool,
) -> Box<dyn Stage> {
    Box::new(WalkAnchors {
        anchor,
        reject,
        drop_lone_letter_dividers,
        splice_orphans_to_prev_block,
    })
}

pub fn walk_anchors_per_lang(rules: Vec<LangAnchorRule>) -> Box<dyn Stage> {
    Box::new(WalkAnchorsPerLang { rules })
}

pub fn pair_bilingual_entries(
    primary_lang: String,
    secondary_lang: String,
    merge_key: String,
) -> Box<dyn Stage> {
    Box::new(PairBilingualEntries {
        primary_lang,
        secondary_lang,
        merge_key,
    })
}

// --- stage structs --------------------------------------------------------

struct SplitPages;
struct OneBlockPerPage {
    default_lang: Option<String>,
}
struct SplitBilingualBlocks;
struct WalkAnchors {
    anchor: AnchorRule,
    reject: Vec<AnchorRule>,
    drop_lone_letter_dividers: bool,
    splice_orphans_to_prev_block: bool,
}
struct WalkAnchorsPerLang {
    rules: Vec<LangAnchorRule>,
}
struct PairBilingualEntries {
    primary_lang: String,
    secondary_lang: String,
    merge_key: String,
}

// --- split_pages ----------------------------------------------------------

impl Stage for SplitPages {
    fn name(&self) -> &str {
        "split_pages"
    }

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let source = data.expect_source(self.name())?;
        let re = Regex::new(r"<!--\s*page\s+(\d+)\s*\(sheet\s+(\d+)\)\s*-->")
            .expect("page marker regex");

        let mut pages = Vec::new();
        let mut last: Option<(u32, u32, usize)> = None; // (page, sheet, body_start)

        for cap in re.captures_iter(&source) {
            let m = cap.get(0).unwrap();
            if let Some((page, sheet, body_start)) = last.take() {
                let text = source[body_start..m.start()].trim().to_string();
                pages.push(Page { page, sheet, text });
            }
            let page = cap.get(1).unwrap().as_str().parse::<u32>().unwrap_or(0);
            let sheet = cap.get(2).unwrap().as_str().parse::<u32>().unwrap_or(0);
            last = Some((page, sheet, m.end()));
        }
        if let Some((page, sheet, body_start)) = last {
            let text = source[body_start..].trim().to_string();
            pages.push(Page { page, sheet, text });
        }

        ctx.coverage.pages = pages.len();
        Ok(StageData::Pages(pages))
    }
}

// --- one_block_per_page ---------------------------------------------------

impl Stage for OneBlockPerPage {
    fn name(&self) -> &str {
        "one_block_per_page"
    }

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let pages = data.expect_pages(self.name())?;
        let blocks: Vec<Block> = pages
            .into_iter()
            .map(|p| {
                let lines: Vec<String> = p
                    .text
                    .lines()
                    .map(|l| l.to_string())
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                Block {
                    page: p.page,
                    sheet: p.sheet,
                    lang: self.default_lang.clone(),
                    lines,
                }
            })
            .collect();
        ctx.coverage.blocks = blocks.len();
        Ok(StageData::Blocks(blocks))
    }
}

// --- split_bilingual_blocks ----------------------------------------------

impl Stage for SplitBilingualBlocks {
    fn name(&self) -> &str {
        "split_bilingual_blocks"
    }

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let pages = data.expect_pages(self.name())?;
        let mut blocks = Vec::new();

        for p in pages {
            let mut current: Option<(Option<String>, Vec<String>)> = None;
            for raw_line in p.text.lines() {
                let line = raw_line.to_string();
                if line.trim().is_empty() {
                    continue;
                }
                let lang = lang_of_line(&line);
                match &mut current {
                    Some((current_lang, lines)) if current_lang.as_deref() == lang.as_deref() => {
                        lines.push(line);
                    }
                    _ => {
                        if let Some((current_lang, lines)) = current.take() {
                            blocks.push(Block {
                                page: p.page,
                                sheet: p.sheet,
                                lang: current_lang,
                                lines,
                            });
                        }
                        current = Some((lang, vec![line]));
                    }
                }
            }
            if let Some((current_lang, lines)) = current.take() {
                blocks.push(Block {
                    page: p.page,
                    sheet: p.sheet,
                    lang: current_lang,
                    lines,
                });
            }
        }

        ctx.coverage.blocks = blocks.len();
        Ok(StageData::Blocks(blocks))
    }
}

fn lang_of_line(line: &str) -> Option<String> {
    let cjk = line.chars().filter(|c| is_cjk(*c)).count();
    let latin = line
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .count();
    if cjk == 0 && latin == 0 {
        None
    } else if cjk >= latin {
        Some("zh".to_string())
    } else {
        Some("en".to_string())
    }
}

// --- walk_anchors ---------------------------------------------------------

impl Stage for WalkAnchors {
    fn name(&self) -> &str {
        "walk_anchors"
    }

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let blocks = data.expect_blocks(self.name())?;
        let mut raws: Vec<RawEntry> = Vec::new();
        let mut unmatched = 0usize;
        for block in blocks {
            walk_block_into(
                &block,
                &self.anchor,
                &self.reject,
                self.drop_lone_letter_dividers,
                self.splice_orphans_to_prev_block,
                &mut raws,
                &mut unmatched,
            );
        }
        ctx.coverage.raws = raws.len();
        ctx.coverage.unmatched_lines = unmatched;
        Ok(StageData::Raws(raws))
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_block_into(
    block: &Block,
    anchor: &AnchorRule,
    reject: &[AnchorRule],
    drop_lone_letter_dividers: bool,
    splice_orphans_to_prev_block: bool,
    raws: &mut Vec<RawEntry>,
    unmatched: &mut usize,
) {
    let mut current: Option<RawEntry> = None;
    let mut orphans: Vec<String> = Vec::new();

    for line in &block.lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if reject.iter().any(|r| r.matches(trimmed)) {
            continue;
        }
        if drop_lone_letter_dividers && is_lone_letter_divider(trimmed) {
            continue;
        }
        if anchor.matches(trimmed) {
            if let Some(prev) = current.take() {
                raws.push(prev);
            }
            current = Some(RawEntry {
                page: block.page,
                sheet: block.sheet,
                anchor: trimmed.to_string(),
                body: Vec::new(),
                lang: block.lang.clone(),
                quality_flags: Vec::new(),
            });
        } else if let Some(entry) = &mut current {
            entry.body.push(line.clone());
        } else {
            orphans.push(line.clone());
        }
    }

    if !orphans.is_empty() {
        if splice_orphans_to_prev_block
            && let Some(prev) = raws.last_mut()
        {
            for o in &orphans {
                prev.body.push(o.clone());
            }
            if !prev
                .quality_flags
                .iter()
                .any(|f| f == "spliced_from_orphan")
            {
                prev.quality_flags.push("spliced_from_orphan".to_string());
            }
        } else {
            *unmatched += orphans.len();
        }
    }

    if let Some(last) = current.take() {
        raws.push(last);
    }
}

fn is_lone_letter_divider(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    chars.len() == 1 && chars[0].is_ascii_alphabetic()
}

// --- walk_anchors_per_lang ------------------------------------------------

impl Stage for WalkAnchorsPerLang {
    fn name(&self) -> &str {
        "walk_anchors_per_lang"
    }

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let blocks = data.expect_blocks(self.name())?;
        let mut raws: Vec<RawEntry> = Vec::new();
        let mut unmatched = 0usize;
        for block in blocks {
            let rule = block
                .lang
                .as_deref()
                .and_then(|l| self.rules.iter().find(|r| r.lang == l));
            match rule {
                Some(r) => walk_block_into(
                    &block,
                    &r.anchor,
                    &r.reject,
                    r.drop_lone_letter_dividers,
                    r.splice_orphans_to_prev_block,
                    &mut raws,
                    &mut unmatched,
                ),
                None => {
                    unmatched += block
                        .lines
                        .iter()
                        .filter(|l| !l.trim().is_empty())
                        .count();
                }
            }
        }
        ctx.coverage.raws = raws.len();
        ctx.coverage.unmatched_lines = unmatched;
        Ok(StageData::Raws(raws))
    }
}

// --- pair_bilingual_entries -----------------------------------------------

impl Stage for PairBilingualEntries {
    fn name(&self) -> &str {
        "pair_bilingual_entries"
    }

    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError> {
        let raws = data.expect_raws(self.name())?;
        let mut out: Vec<RawEntry> = Vec::new();
        let mut mismatches = 0usize;

        let mut i = 0usize;
        while i < raws.len() {
            let a = &raws[i];
            let a_lang = a.lang.as_deref().unwrap_or("");
            if a_lang == self.primary_lang
                && i + 1 < raws.len()
                && raws[i + 1].lang.as_deref().unwrap_or("") == self.secondary_lang
            {
                let b = &raws[i + 1];
                let merged_body = format!(
                    "{}<<<{}_head>>>{}<<<{}_body>>>{}",
                    a.body.join(" "),
                    self.merge_key,
                    b.anchor,
                    self.merge_key,
                    b.body.join(" ")
                );
                out.push(RawEntry {
                    page: a.page,
                    sheet: a.sheet,
                    anchor: a.anchor.clone(),
                    body: vec![merged_body],
                    lang: a.lang.clone(),
                    quality_flags: a.quality_flags.clone(),
                });
                i += 2;
            } else {
                mismatches += 1;
                let mut solo = a.clone();
                if !solo.quality_flags.iter().any(|f| f == "pair_mismatch") {
                    solo.quality_flags.push("pair_mismatch".to_string());
                }
                out.push(solo);
                i += 1;
            }
        }

        ctx.coverage.pair_mismatch = mismatches;
        ctx.coverage.raws = out.len();
        Ok(StageData::Raws(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::AnchorRule;

    fn run_stage(stage: Box<dyn Stage>, data: StageData) -> (StageData, Ctx) {
        let mut ctx = Ctx::new();
        let out = stage.run(data, &mut ctx).expect("stage run");
        (out, ctx)
    }

    // ---- split_pages ----

    #[test]
    fn split_pages_cuts_at_page_markers_and_records_page_and_sheet() {
        let source = "<!-- page 1 (sheet 1) -->\n\
                      content of page 1\n\
                      Smith\n\
                      <!-- page 2 (sheet 2) -->\n\
                      content of page 2\n\
                      Jones";
        let (out, ctx) = run_stage(split_pages(), StageData::Source(source.to_string()));
        let pages = match out {
            StageData::Pages(p) => p,
            other => panic!("expected Pages, got {other:?}"),
        };
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].page, 1);
        assert_eq!(pages[0].sheet, 1);
        assert!(pages[0].text.contains("content of page 1"));
        assert_eq!(pages[1].page, 2);
        assert_eq!(ctx.coverage.pages, 2);
    }

    // ---- one_block_per_page ----

    #[test]
    fn one_block_per_page_makes_one_block_per_input_page_and_carries_lang() {
        let pages = vec![
            Page {
                page: 1,
                sheet: 1,
                text: "line A\nline B".to_string(),
            },
            Page {
                page: 2,
                sheet: 2,
                text: "line C".to_string(),
            },
        ];
        let (out, ctx) = run_stage(
            one_block_per_page(Some("latin".to_string())),
            StageData::Pages(pages),
        );
        let blocks = match out {
            StageData::Blocks(b) => b,
            other => panic!("expected Blocks, got {other:?}"),
        };
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].lines.len(), 2);
        assert_eq!(blocks[0].lang.as_deref(), Some("latin"));
        assert_eq!(ctx.coverage.blocks, 2);
    }

    // ---- split_bilingual_blocks ----

    #[test]
    fn split_bilingual_blocks_emits_a_new_block_at_every_script_flip() {
        let page = Page {
            page: 1,
            sheet: 1,
            text: "philosophical knowledge\n\
                   \u{54F2}\u{5B66}\u{77E5}\u{8BC6}\n\
                   relating to philosophy"
                .to_string(),
        };
        let (out, ctx) = run_stage(
            split_bilingual_blocks(),
            StageData::Pages(vec![page]),
        );
        let blocks = match out {
            StageData::Blocks(b) => b,
            other => panic!("expected Blocks, got {other:?}"),
        };
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].lang.as_deref(), Some("en"));
        assert_eq!(blocks[1].lang.as_deref(), Some("zh"));
        assert_eq!(blocks[2].lang.as_deref(), Some("en"));
        assert_eq!(ctx.coverage.blocks, 3);
    }

    // ---- walk_anchors ----

    fn block(lines: Vec<&str>, lang: Option<&str>) -> Block {
        Block {
            page: 1,
            sheet: 1,
            lang: lang.map(String::from),
            lines: lines.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn walk_anchors_groups_body_lines_under_each_anchor() {
        // Body lines lead with lowercase or digits so the
        // four-word-cap LatinHeadword rule lets them through as
        // body rather than re-anchoring.
        let blocks = vec![block(
            vec![
                "Smith",
                "1900-2000",
                "an american baseball player",
                "Jones",
                "a british poet",
            ],
            Some("latin"),
        )];
        let (out, ctx) = run_stage(
            walk_anchors(AnchorRule::LatinHeadword, vec![], false, false),
            StageData::Blocks(blocks),
        );
        let raws = match out {
            StageData::Raws(r) => r,
            other => panic!("expected Raws, got {other:?}"),
        };
        assert_eq!(raws.len(), 2);
        assert_eq!(raws[0].anchor, "Smith");
        assert_eq!(
            raws[0].body,
            vec!["1900-2000", "an american baseball player"]
        );
        assert_eq!(raws[1].anchor, "Jones");
        assert_eq!(raws[1].body, vec!["a british poet"]);
        assert_eq!(ctx.coverage.raws, 2);
    }

    #[test]
    fn walk_anchors_drops_lone_letter_dividers_when_enabled() {
        let blocks = vec![block(
            vec!["A", "Smith", "biographical"],
            Some("latin"),
        )];
        let (out, _) = run_stage(
            walk_anchors(AnchorRule::LatinHeadword, vec![], true, false),
            StageData::Blocks(blocks),
        );
        let raws = match out {
            StageData::Raws(r) => r,
            other => panic!("expected Raws, got {other:?}"),
        };
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].anchor, "Smith");
    }

    #[test]
    fn walk_anchors_splices_orphans_to_prev_block_when_enabled() {
        let blocks = vec![
            block(vec!["Smith", "1900-2000"], Some("latin")),
            // Page-break continuation: pure-digit and lowercase body
            // lines fall through anchor matching and become orphans.
            block(vec!["page-break continuation", "1850-1900"], Some("latin")),
        ];
        let (out, ctx) = run_stage(
            walk_anchors(AnchorRule::LatinHeadword, vec![], false, true),
            StageData::Blocks(blocks),
        );
        let raws = match out {
            StageData::Raws(r) => r,
            other => panic!("expected Raws, got {other:?}"),
        };
        assert_eq!(raws.len(), 1);
        assert!(
            raws[0].body.iter().any(|l| l == "page-break continuation"),
            "orphan body line must splice onto Smith: {:?}",
            raws[0].body
        );
        assert!(
            raws[0].body.iter().any(|l| l == "1850-1900"),
            "orphan body line must splice onto Smith: {:?}",
            raws[0].body
        );
        assert!(
            raws[0]
                .quality_flags
                .iter()
                .any(|f| f == "spliced_from_orphan"),
            "spliced raw must carry the spliced_from_orphan flag"
        );
        assert_eq!(ctx.coverage.unmatched_lines, 0);
    }

    #[test]
    fn walk_anchors_records_unmatched_when_splice_is_off() {
        let blocks = vec![
            block(vec!["Smith", "1900-2000"], Some("latin")),
            block(vec!["page-break continuation", "1850-1900"], Some("latin")),
        ];
        let (out, ctx) = run_stage(
            walk_anchors(AnchorRule::LatinHeadword, vec![], false, false),
            StageData::Blocks(blocks),
        );
        let raws = match out {
            StageData::Raws(r) => r,
            other => panic!("expected Raws, got {other:?}"),
        };
        // Smith's entry stays unchanged; the orphans bump the
        // unmatched_lines counter rather than splicing.
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].body, vec!["1900-2000"]);
        assert_eq!(ctx.coverage.unmatched_lines, 2);
    }

    // ---- walk_anchors_per_lang ----

    #[test]
    fn walk_anchors_per_lang_runs_each_block_with_its_own_rule_in_input_order() {
        let blocks = vec![
            block(vec!["philosophical knowledge"], Some("en")),
            block(vec!["\u{54F2}\u{5B66}\u{77E5}\u{8BC6}"], Some("zh")),
            block(vec!["relating to philosophy"], Some("en")),
        ];
        let rules = vec![
            LangAnchorRule {
                lang: "en".to_string(),
                anchor: AnchorRule::EnglishShortHeadword { max_words: 4 },
                reject: vec![],
                drop_lone_letter_dividers: false,
                splice_orphans_to_prev_block: false,
            },
            LangAnchorRule {
                lang: "zh".to_string(),
                anchor: AnchorRule::CjkShortHeadword { max_chars: 6 },
                reject: vec![],
                drop_lone_letter_dividers: false,
                splice_orphans_to_prev_block: false,
            },
        ];
        let (out, _) = run_stage(
            walk_anchors_per_lang(rules),
            StageData::Blocks(blocks),
        );
        let raws = match out {
            StageData::Raws(r) => r,
            other => panic!("expected Raws, got {other:?}"),
        };
        assert_eq!(raws.len(), 3);
        assert_eq!(raws[0].anchor, "philosophical knowledge");
        assert_eq!(raws[1].anchor, "\u{54F2}\u{5B66}\u{77E5}\u{8BC6}");
        assert_eq!(raws[2].anchor, "relating to philosophy");
    }

    // ---- pair_bilingual_entries ----

    fn raw(anchor: &str, body: Vec<&str>, lang: &str) -> RawEntry {
        RawEntry {
            page: 1,
            sheet: 1,
            anchor: anchor.to_string(),
            body: body.into_iter().map(String::from).collect(),
            lang: Some(lang.to_string()),
            quality_flags: vec![],
        }
    }

    #[test]
    fn pair_bilingual_entries_merges_adjacent_pairs_and_packs_body() {
        let raws = vec![
            raw("philosophical", vec!["relating to philosophy"], "en"),
            raw(
                "\u{54F2}\u{5B66}\u{7684}",
                vec!["\u{5173}\u{4E8E}\u{54F2}\u{5B66}\u{7684}"],
                "zh",
            ),
        ];
        let (out, ctx) = run_stage(
            pair_bilingual_entries(
                "en".to_string(),
                "zh".to_string(),
                "translation".to_string(),
            ),
            StageData::Raws(raws),
        );
        let merged = match out {
            StageData::Raws(r) => r,
            other => panic!("expected Raws, got {other:?}"),
        };
        assert_eq!(merged.len(), 1);
        let body = merged[0].body.first().expect("merged body");
        assert!(body.contains("<<<translation_head>>>"));
        assert!(body.contains("<<<translation_body>>>"));
        assert!(body.starts_with("relating to philosophy"));
        assert_eq!(ctx.coverage.pair_mismatch, 0);
    }

    #[test]
    fn pair_bilingual_entries_flags_unmatched_primary_with_pair_mismatch() {
        let raws = vec![
            raw("philosophical", vec!["relating to philosophy"], "en"),
            // Next is also en, no zh follow-up: the primary above
            // can't pair and emits with pair_mismatch.
            raw("knowledge", vec!["fact known to be true"], "en"),
        ];
        let (out, ctx) = run_stage(
            pair_bilingual_entries(
                "en".to_string(),
                "zh".to_string(),
                "translation".to_string(),
            ),
            StageData::Raws(raws),
        );
        let merged = match out {
            StageData::Raws(r) => r,
            other => panic!("expected Raws, got {other:?}"),
        };
        assert_eq!(merged.len(), 2);
        for r in &merged {
            assert!(
                r.quality_flags.iter().any(|f| f == "pair_mismatch"),
                "unmatched primary must carry pair_mismatch: {:?}",
                r.quality_flags
            );
        }
        assert_eq!(ctx.coverage.pair_mismatch, 2);
    }
}
