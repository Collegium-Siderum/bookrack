// SPDX-License-Identifier: Apache-2.0

//! The `Stage` trait and the `Pipeline` runner.
//!
//! A [`Pipeline`] is a named, ordered sequence of [`Stage`]s. Each
//! stage takes the previous [`crate::core::StageData`] variant and
//! returns the next; the pipeline asserts that the final output is
//! `Drafts` and stamps `coverage.entries` from the draft count.

use crate::core::{Coverage, Ctx, EntryDraft, StageData};
use crate::error::ParseError;

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
                &self
                    .stages
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<&str>>(),
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
            data = stage.run(data, &mut ctx)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EntryDraft;
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
}
