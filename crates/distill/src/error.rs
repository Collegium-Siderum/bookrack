// SPDX-License-Identifier: Apache-2.0

//! Distill error surface.
//!
//! The variants cover both runtime pipeline failures (`StageMismatch`)
//! and book.toml load-time failures (`StageNotFound`, `TomlParse`,
//! `CatalogViolation`, and the two forbidden-form references). Phase 3
//! emits only `StageMismatch`; phases 4 and 7 wire the rest as the
//! catalog loader and the book.toml dispatcher land.

/// All failures the distill pipeline can surface.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// A stage received a `StageData` variant it cannot consume, or
    /// the pipeline's final output is not `Drafts`. `stage` is the
    /// implementer's name (or `<pipeline:NAME>` for the pipeline-level
    /// final check); `expected` and `actual` are the lower-case
    /// variant names from [`crate::core::StageData::kind`].
    #[error("stage {stage:?} expected {expected} input, got {actual}")]
    StageMismatch {
        stage: String,
        expected: &'static str,
        actual: &'static str,
    },

    /// A book.toml `parser.stages` entry referenced a stage that is
    /// not declared in `stage_catalog.toml`. Surfaced by the loader
    /// added in phase 4.
    #[error("stage {0:?} is not registered in the stage_catalog")]
    StageNotFound(String),

    /// `book.toml` failed to parse.
    #[error("book.toml parse error: {0}")]
    TomlParse(String),

    /// A `book.toml` declaration violated one of the three controlled
    /// vocabularies (property catalog, stage catalog, quality flags).
    /// Phase 4 attaches the specific vocabulary name to the message.
    #[error("catalog violation: {0}")]
    CatalogViolation(String),

    /// A `book.toml` referenced a stage with the `@script::<fn>`
    /// escape-hatch syntax. The hatch is reserved for a future
    /// embedded scripting engine; the loader in phase 7 fails the
    /// build with this variant.
    #[error(
        "@script::{0} stage references are not implemented; \
         see the v2 distill execution manual §1.4"
    )]
    ScriptRefForbidden(String),

    /// A `book.toml` referenced a stage with the `@llm::<fn>` form.
    /// The LLM-assist hook is mother doc §8.1, deferred past v1; the
    /// loader in phase 7 fails the build with this variant.
    #[error(
        "@llm::{0} stage references are not implemented; \
         see the v2 distill investigation doc §8.1"
    )]
    LlmHookNotImplemented(String),

    /// A `<!-- page N (sheet M) -->` marker carried a number that did
    /// not parse as a `u32`. The marker text and source byte offset
    /// are both preserved so the operator can locate the bad line in
    /// the original source. The previous code path silently collapsed
    /// the offending marker to page / sheet `0`, mixing later content
    /// into a phantom first page.
    #[error("invalid page marker at byte {byte_offset}: {marker:?}")]
    InvalidPageMarker { marker: String, byte_offset: usize },

    /// A `book.toml` `regex` pattern reference failed to compile.
    /// Surfaced by the dispatcher at load time so the operator sees
    /// the broken rule before the pipeline silently collapses every
    /// match to "no match" — the historical `Regex::new(...).ok()?`
    /// shape used at runtime would render a bad pattern as a
    /// quiet skip.
    #[error("invalid regex pattern {pattern:?}: {reason}")]
    InvalidPattern { pattern: String, reason: String },
}
