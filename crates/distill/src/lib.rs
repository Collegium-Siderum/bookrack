// SPDX-License-Identifier: Apache-2.0

//! The distill pipeline.
//!
//! Takes the polyocr Markdown of one reference book through a
//! catalog-declared sequence of stages and produces the
//! `EntryDraft`s the [`bookrack_refs`](../bookrack_refs/index.html)
//! crate writes into `reference.db`. Phase 3 ships the framework
//! (core types, [`Stage`] trait, [`Pipeline`] runner, [`ParseError`])
//! with no built-in stages and no book.toml loader; phases 4–7 land
//! the controlled vocabularies, the builtin stages, and the
//! dispatcher.

pub mod book_toml;
pub mod catalogs;
pub mod core;
pub mod error;
pub mod extractor;
pub mod finalize;
pub mod patterns;
pub mod pipeline;
pub mod splitter;

pub use book_toml::{BookToml, IndexEntry, ParserSection, StageConfig, StageRef};
pub use catalogs::{
    Catalogs, FlagSpec, ParamSpec, PropertyCatalog, PropertySpec, QualityFlagCatalog, StageCatalog,
    StageSpec,
};
pub use core::{Block, Coverage, Ctx, EntryDraft, Page, RawEntry, SplitEntry, StageData};
pub use error::ParseError;
pub use extractor::{
    SearchTarget, extract_bracketed_tag, extract_gender_tag, extract_quotes, extract_year_span,
    partition_body_around_match, split_variants, unpack_paired_body,
};
pub use finalize::{FtsComposer, KeyNormalizer, to_entry_draft};
pub use patterns::{BracketKind, PatternMatch, PatternRef, match_pattern};
pub use pipeline::{Pipeline, Stage};
pub use splitter::{split_at_first_cjk, split_headline_only};
