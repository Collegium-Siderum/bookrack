// SPDX-License-Identifier: Apache-2.0

//! The `Stage` trait and the `Pipeline` runner.
//!
//! A [`Pipeline`] is a named, ordered sequence of [`Stage`]s. Each
//! stage takes the previous [`crate::core::StageData`] variant and
//! returns the next; the pipeline asserts that the final output is
//! `Drafts` and stamps `coverage.entries` from the draft count.

use std::collections::HashSet;

use crate::core::{Coverage, Ctx, EntryDraft, StageData, StageReport};
use crate::error::ParseError;

/// Items above this count skip the dropped-line sample step. The
/// per-stage retention number still gets recorded; only the
/// best-effort sample is suppressed, because [`snapshot_items`] would
/// otherwise format every input twice on the largest inputs.
const MAX_SAMPLE_SCAN: usize = 10_000;

/// One processing stage in the distill pipeline. Object-safe: no
/// generics, no `Self` in signatures past the receiver, `Send + Sync`
/// for the orchestrator that holds the pipeline behind an `Arc`.
pub trait Stage: Send + Sync {
    /// Stable string used in error messages and in audit logs. Matches
    /// the stage_catalog entry name.
    fn name(&self) -> &str;

    /// Consume one `StageData` variant and emit the next. Implementers
    /// should assert the expected input via
    /// [`StageData::expect_source`] and friends, which raise
    /// `ParseError::StageMismatch` with the stage name plumbed in.
    fn run(&self, data: StageData, ctx: &mut Ctx) -> Result<StageData, ParseError>;
}

/// A named pipeline: zero or more stages run in order, starting from
/// `StageData::Source` and required to end at `StageData::Drafts`.
pub struct Pipeline {
    name: String,
    stages: Vec<Box<dyn Stage>>,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("name", &self.name)
            .field(
                "stages",
                &self.stages.iter().map(|s| s.name()).collect::<Vec<&str>>(),
            )
            .finish()
    }
}

impl Pipeline {
    /// Build an empty pipeline. Useful for chaining `.push` from a
    /// catalog-driven loader.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            stages: Vec::new(),
        }
    }

    /// Append one stage. Returns `&mut self` so loaders can fluently
    /// build a pipeline from a `Vec` of catalog entries.
    pub fn push(&mut self, stage: Box<dyn Stage>) -> &mut Self {
        self.stages.push(stage);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn len(&self) -> usize {
        self.stages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Drive the pipeline over one source string with a default
    /// (empty) [`Ctx::extras`]. See [`Pipeline::run_with_extras`] for
    /// the variant that seeds `book_slug` / `distill_run_id` /
    /// `ocr_engine` into the context so the finalize stage can stamp
    /// them onto `EntryDraft::source`.
    pub fn run(&self, source: String) -> Result<(Vec<EntryDraft>, Coverage), ParseError> {
        self.run_with_extras(source, serde_json::Map::new())
    }

    /// Drive the pipeline with a pre-populated `Ctx::extras` map.
    /// CLI callers stash `book_slug`, `distill_run_id`, and
    /// `ocr_engine` strings here; the finalize stage reads them when
    /// composing `EntryDraft::source`.
    pub fn run_with_extras(
        &self,
        source: String,
        extras: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Vec<EntryDraft>, Coverage), ParseError> {
        let mut data = StageData::Source(source);
        let mut ctx = Ctx::new();
        ctx.extras = extras;
        for stage in &self.stages {
            let in_kind = data.kind();
            let in_len = cardinality(&data);
            let in_items = if in_len <= MAX_SAMPLE_SCAN {
                snapshot_items(&data)
            } else {
                Vec::new()
            };
            data = stage.run(data, &mut ctx)?;
            let out_kind = data.kind();
            let out_len = cardinality(&data);
            let dropped_sample =
                collect_dropped_sample(in_kind, &in_items, out_kind, out_len, &data);
            ctx.coverage.stage_reports.push(StageReport {
                stage_name: stage.name().to_string(),
                in_kind,
                in_len,
                out_kind,
                out_len,
                dropped_sample,
            });
        }
        match data {
            StageData::Drafts(drafts) => {
                ctx.coverage.entries = drafts.len();
                Ok((drafts, ctx.coverage))
            }
            other => Err(ParseError::StageMismatch {
                stage: format!("<pipeline:{}>", self.name),
                expected: "drafts",
                actual: other.kind(),
            }),
        }
    }
}

/// Number of items addressable inside a [`StageData`]. `Source` always
/// reports `1` (the whole string is one item from the pipeline's
/// point of view); every other variant reports its `Vec` length.
fn cardinality(data: &StageData) -> usize {
    match data {
        StageData::Source(_) => 1,
        StageData::Pages(v) => v.len(),
        StageData::Blocks(v) => v.len(),
        StageData::Raws(v) => v.len(),
        StageData::Splits(v) => v.len(),
        StageData::Drafts(v) => v.len(),
    }
}

/// Format every item inside a [`StageData`] with `Debug`, truncating
/// each entry to a bounded number of characters so a runaway body
/// cannot blow up the report. `Source` collapses to an empty vec
/// because the diff step only compares same-kind variants.
fn snapshot_items(data: &StageData) -> Vec<String> {
    match data {
        StageData::Source(_) => Vec::new(),
        StageData::Pages(v) => v.iter().map(truncate_debug).collect(),
        StageData::Blocks(v) => v.iter().map(truncate_debug).collect(),
        StageData::Raws(v) => v.iter().map(truncate_debug).collect(),
        StageData::Splits(v) => v.iter().map(truncate_debug).collect(),
        StageData::Drafts(v) => v.iter().map(truncate_debug).collect(),
    }
}

fn truncate_debug<T: std::fmt::Debug>(item: &T) -> String {
    const MAX_CHARS: usize = 120;
    let s = format!("{item:?}");
    let mut out = String::with_capacity(s.len().min(MAX_CHARS * 4) + 3);
    for (i, c) in s.chars().enumerate() {
        if i >= MAX_CHARS {
            out.push_str("...");
            return out;
        }
        out.push(c);
    }
    out
}

/// Best-effort: when the input and output variants match and the
/// stage shrank the collection, list up to three input items whose
/// `truncate_debug` form does not appear in the output. Same-kind
/// stages that only mutate items still emit no sample because every
/// truncated form changes alongside the field. Cross-kind stages
/// always return an empty sample.
fn collect_dropped_sample(
    in_kind: &'static str,
    in_items: &[String],
    out_kind: &'static str,
    out_len: usize,
    out_data: &StageData,
) -> Vec<String> {
    if in_kind != out_kind || in_items.is_empty() || out_len >= in_items.len() {
        return Vec::new();
    }
    let out_items = snapshot_items(out_data);
    let out_set: HashSet<&str> = out_items.iter().map(String::as_str).collect();
    in_items
        .iter()
        .filter(|s| !out_set.contains(s.as_str()))
        .take(3)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{EntryDraft, Page};
    use serde_json::{Map, json};

    /// A stage that hands its input back unchanged. Useful to assert
    /// that a non-`Drafts`-terminating pipeline still trips the
    /// final-output check.
    struct IdentityStage;
    impl Stage for IdentityStage {
        fn name(&self) -> &str {
            "identity"
        }
        fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
            Ok(data)
        }
    }

    /// A stage that jumps straight from `Source` to a synthetic
    /// `Drafts` of `n` entries, so the pipeline's success branch can
    /// be exercised without any of the real builtins.
    struct MockSourceToDrafts {
        n: usize,
        book_slug: String,
    }
    impl Stage for MockSourceToDrafts {
        fn name(&self) -> &str {
            "mock_source_to_drafts"
        }
        fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
            let _ = data.expect_source(self.name())?;
            let drafts = (0..self.n)
                .map(|i| EntryDraft {
                    book_slug: self.book_slug.clone(),
                    entry_key: format!("k{i}"),
                    headword: format!("H{i}"),
                    aliases: vec![],
                    payload: Map::new(),
                    fts_text: format!("H{i}"),
                    source: json!({"book_slug": self.book_slug, "page": 1}),
                    quality_flags: vec![],
                })
                .collect();
            Ok(StageData::Drafts(drafts))
        }
    }

    #[test]
    fn an_empty_pipeline_cannot_reach_drafts() {
        let pipeline = Pipeline::new("empty");
        let err = pipeline.run(String::new()).unwrap_err();
        match err {
            ParseError::StageMismatch {
                stage,
                expected,
                actual,
            } => {
                assert!(
                    stage.starts_with("<pipeline:empty>"),
                    "stage label must name the pipeline: {stage}"
                );
                assert_eq!(expected, "drafts");
                assert_eq!(actual, "source");
            }
            other => panic!("expected StageMismatch, got {other:?}"),
        }
    }

    #[test]
    fn an_identity_only_pipeline_still_fails_the_final_output_check() {
        let mut pipeline = Pipeline::new("identity_only");
        pipeline.push(Box::new(IdentityStage));
        let err = pipeline.run(String::new()).unwrap_err();
        assert!(matches!(
            err,
            ParseError::StageMismatch {
                expected: "drafts",
                actual: "source",
                ..
            }
        ));
    }

    #[test]
    fn a_source_to_drafts_pipeline_returns_the_drafts_and_stamps_entries_count() {
        let mut pipeline = Pipeline::new("mock");
        pipeline.push(Box::new(MockSourceToDrafts {
            n: 3,
            book_slug: "fake_book".to_string(),
        }));
        let (drafts, coverage) = pipeline.run(String::new()).expect("pipeline run");
        assert_eq!(drafts.len(), 3);
        assert_eq!(coverage.entries, 3);
        // No unmatched lines were reported by the mock stage, so
        // coverage_pct collapses to the 100% no-loss case.
        assert!((coverage.coverage_pct() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_stage_that_receives_the_wrong_variant_emits_stage_mismatch_with_its_own_name() {
        let stage = MockSourceToDrafts {
            n: 1,
            book_slug: "fake_book".to_string(),
        };
        let mut ctx = Ctx::new();
        let err = stage.run(StageData::Pages(vec![]), &mut ctx).unwrap_err();
        match err {
            ParseError::StageMismatch {
                stage: name,
                expected,
                actual,
            } => {
                assert_eq!(name, "mock_source_to_drafts");
                assert_eq!(expected, "source");
                assert_eq!(actual, "pages");
            }
            other => panic!("expected StageMismatch, got {other:?}"),
        }
    }

    /// Seed a synthetic [`StageData::Pages`] value, used as the first
    /// rung of the report-instrumentation test below.
    struct SeedPages {
        pages: Vec<Page>,
    }
    impl Stage for SeedPages {
        fn name(&self) -> &str {
            "seed_pages"
        }
        fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
            let _ = data.expect_source(self.name())?;
            Ok(StageData::Pages(self.pages.clone()))
        }
    }

    /// Drop every odd-indexed page, shrinking a same-kind variant so
    /// the dropped-sample diff has something to find.
    struct KeepEvenIndexed;
    impl Stage for KeepEvenIndexed {
        fn name(&self) -> &str {
            "keep_even_indexed"
        }
        fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
            let pages = data.expect_pages(self.name())?;
            let kept = pages
                .into_iter()
                .enumerate()
                .filter(|(i, _)| i % 2 == 0)
                .map(|(_, p)| p)
                .collect();
            Ok(StageData::Pages(kept))
        }
    }

    /// Finalise a synthetic Pages stream into a Drafts stream that
    /// satisfies the pipeline's terminal-variant check.
    struct PagesToDrafts;
    impl Stage for PagesToDrafts {
        fn name(&self) -> &str {
            "pages_to_drafts"
        }
        fn run(&self, data: StageData, _ctx: &mut Ctx) -> Result<StageData, ParseError> {
            let pages = data.expect_pages(self.name())?;
            let drafts = pages
                .into_iter()
                .map(|p| EntryDraft {
                    book_slug: "x".to_string(),
                    entry_key: format!("k{}", p.page),
                    headword: format!("h{}", p.page),
                    aliases: vec![],
                    payload: Map::new(),
                    fts_text: format!("h{}", p.page),
                    source: json!({"book_slug": "x", "page": p.page}),
                    quality_flags: vec![],
                })
                .collect();
            Ok(StageData::Drafts(drafts))
        }
    }

    /// The per-stage report block has one entry per stage and records
    /// cardinality on both sides of every call. A same-kind stage
    /// that shrinks its input also gets a small sample of items that
    /// did not survive.
    #[test]
    fn stage_reports_capture_cardinality_and_dropped_samples() {
        let mut pipeline = Pipeline::new("report_demo");
        let seed = vec![
            Page {
                page: 1,
                sheet: 1,
                text: "alpha".to_string(),
            },
            Page {
                page: 2,
                sheet: 2,
                text: "beta".to_string(),
            },
            Page {
                page: 3,
                sheet: 3,
                text: "gamma".to_string(),
            },
            Page {
                page: 4,
                sheet: 4,
                text: "delta".to_string(),
            },
        ];
        pipeline.push(Box::new(SeedPages { pages: seed }));
        pipeline.push(Box::new(KeepEvenIndexed));
        pipeline.push(Box::new(PagesToDrafts));

        let (_, coverage) = pipeline.run(String::new()).expect("pipeline run");
        assert_eq!(coverage.stage_reports.len(), 3);

        let seed_row = &coverage.stage_reports[0];
        assert_eq!(seed_row.stage_name, "seed_pages");
        assert_eq!(seed_row.in_kind, "source");
        assert_eq!(seed_row.in_len, 1);
        assert_eq!(seed_row.out_kind, "pages");
        assert_eq!(seed_row.out_len, 4);
        assert!(seed_row.retention().is_none());
        assert!(seed_row.dropped_sample.is_empty());

        let filter_row = &coverage.stage_reports[1];
        assert_eq!(filter_row.stage_name, "keep_even_indexed");
        assert_eq!(filter_row.in_kind, "pages");
        assert_eq!(filter_row.in_len, 4);
        assert_eq!(filter_row.out_kind, "pages");
        assert_eq!(filter_row.out_len, 2);
        assert_eq!(filter_row.retention(), Some(0.5));
        // The filter keeps indices 0 and 2 (pages 1 and 3); the
        // sample must therefore mention the dropped pages 2 and 4.
        assert_eq!(filter_row.dropped_sample.len(), 2);
        assert!(filter_row.dropped_sample.iter().any(|s| s.contains("beta")));
        assert!(
            filter_row
                .dropped_sample
                .iter()
                .any(|s| s.contains("delta"))
        );

        let finalize_row = &coverage.stage_reports[2];
        assert_eq!(finalize_row.stage_name, "pages_to_drafts");
        assert_eq!(finalize_row.in_kind, "pages");
        assert_eq!(finalize_row.in_len, 2);
        assert_eq!(finalize_row.out_kind, "drafts");
        assert_eq!(finalize_row.out_len, 2);
        assert!(finalize_row.dropped_sample.is_empty());
    }
}
