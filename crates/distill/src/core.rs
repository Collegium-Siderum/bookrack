// SPDX-License-Identifier: Apache-2.0

//! Core data types flowing through the distill pipeline.
//!
//! [`StageData`] is the discriminated union threaded from one
//! [`crate::pipeline::Stage`] to the next. Each variant corresponds to
//! one level of refinement, from raw OCR text down to ready-to-write
//! `EntryDraft`s. [`Coverage`] is the per-run metric block that records
//! how much of the input made it through; [`Ctx`] bundles the coverage
//! with a free-form `extras` map for stages that need scratch space.

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::error::ParseError;

/// One source page extracted from the polyocr Markdown.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// The book-internal page number printed on the page marker.
    pub page: u32,
    /// The OCR sheet number assigned at scan time (PDF page index).
    pub sheet: u32,
    /// The page's Markdown body, including all anchors and body lines.
    pub text: String,
}

/// One contiguous block of lines on a page, tagged with the script
/// the segmenter believes it carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub page: u32,
    pub sheet: u32,
    /// `Some("latin")` / `Some("cjk")` / `Some("en")` / `Some("zh")` /
    /// `None` for unknown. The string is the same one referenced from
    /// book.toml `[parser]` rules.
    pub lang: Option<String>,
    pub lines: Vec<String>,
}

/// One headword-anchored run cut out of a block: the anchor line plus
/// the body lines that follow it up to the next anchor or block end.
///
/// `quality_flags` carries flags stamped at the raw level (e.g.
/// `pair_mismatch` from `pair_bilingual_entries`,
/// `spliced_from_orphan` from `walk_anchors`) and is forwarded onto
/// the `SplitEntry` by both splitter stages.
#[derive(Debug, Clone, PartialEq)]
pub struct RawEntry {
    pub page: u32,
    pub sheet: u32,
    pub anchor: String,
    pub body: Vec<String>,
    pub lang: Option<String>,
    pub quality_flags: Vec<String>,
}

/// One entry after head / body separation and any payload-field
/// extraction. Still pre-finalize: the `payload` keys are not yet
/// validated against the property catalog and the entry has no
/// `entry_key` or FTS text.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitEntry {
    pub page: u32,
    pub sheet: u32,
    pub headword: String,
    pub body: String,
    pub lang: Option<String>,
    pub payload: JsonMap<String, JsonValue>,
    pub quality_flags: Vec<String>,
}

/// The pipeline's final per-entry product, in the exact shape
/// `Refs::upsert_entry` consumes.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryDraft {
    pub book_slug: String,
    pub entry_key: String,
    pub headword: String,
    pub aliases: Vec<String>,
    pub payload: JsonMap<String, JsonValue>,
    pub fts_text: String,
    pub source: JsonValue,
    pub quality_flags: Vec<String>,
}

/// The discriminated union of every level of refinement a stage may
/// emit or consume. Stages are responsible for asserting the variant
/// they expect via [`StageData::expect_source`] and friends.
#[derive(Debug, Clone)]
pub enum StageData {
    Source(String),
    Pages(Vec<Page>),
    Blocks(Vec<Block>),
    Raws(Vec<RawEntry>),
    Splits(Vec<SplitEntry>),
    Drafts(Vec<EntryDraft>),
}

impl StageData {
    /// Lower-case variant name, suitable for error messages and the
    /// stage_catalog `input` / `output` strings.
    pub fn kind(&self) -> &'static str {
        match self {
            StageData::Source(_) => "source",
            StageData::Pages(_) => "pages",
            StageData::Blocks(_) => "blocks",
            StageData::Raws(_) => "raws",
            StageData::Splits(_) => "splits",
            StageData::Drafts(_) => "drafts",
        }
    }

    pub fn expect_source(self, stage: &str) -> Result<String, ParseError> {
        match self {
            StageData::Source(s) => Ok(s),
            other => Err(mismatch(stage, "source", other.kind())),
        }
    }

    pub fn expect_pages(self, stage: &str) -> Result<Vec<Page>, ParseError> {
        match self {
            StageData::Pages(ps) => Ok(ps),
            other => Err(mismatch(stage, "pages", other.kind())),
        }
    }

    pub fn expect_blocks(self, stage: &str) -> Result<Vec<Block>, ParseError> {
        match self {
            StageData::Blocks(bs) => Ok(bs),
            other => Err(mismatch(stage, "blocks", other.kind())),
        }
    }

    pub fn expect_raws(self, stage: &str) -> Result<Vec<RawEntry>, ParseError> {
        match self {
            StageData::Raws(rs) => Ok(rs),
            other => Err(mismatch(stage, "raws", other.kind())),
        }
    }

    pub fn expect_splits(self, stage: &str) -> Result<Vec<SplitEntry>, ParseError> {
        match self {
            StageData::Splits(ss) => Ok(ss),
            other => Err(mismatch(stage, "splits", other.kind())),
        }
    }

    pub fn expect_drafts(self, stage: &str) -> Result<Vec<EntryDraft>, ParseError> {
        match self {
            StageData::Drafts(ds) => Ok(ds),
            other => Err(mismatch(stage, "drafts", other.kind())),
        }
    }
}

fn mismatch(stage: &str, expected: &'static str, actual: &'static str) -> ParseError {
    ParseError::StageMismatch {
        stage: stage.to_string(),
        expected,
        actual,
    }
}

/// Per-run metric block updated by stages as they consume their input.
///
/// `pages`, `blocks`, and the lower counters are written by the
/// matching segment / walker stages; `entries` is finalised by the
/// pipeline at the end of a successful run as the count of emitted
/// `EntryDraft`s. `coverage_pct` reports how much of the candidate
/// input the pipeline actually turned into structured entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Coverage {
    pub pages: usize,
    pub blocks: usize,
    pub raws: usize,
    pub splits: usize,
    pub entries: usize,
    pub unmatched_lines: usize,
    pub pair_mismatch: usize,
}

impl Coverage {
    /// Ratio of entries actually produced to total candidates seen
    /// (entries plus the lines that fell out as unmatched), expressed
    /// as a percentage. A pipeline that swept the input cleanly scores
    /// `100.0`; one that dropped every candidate scores `0.0`. An
    /// empty run (no candidates either way) reports `100.0` so the
    /// metric does not falsely flag a no-op as broken.
    pub fn coverage_pct(&self) -> f64 {
        let total = self.entries + self.unmatched_lines;
        if total == 0 {
            100.0
        } else {
            100.0 * self.entries as f64 / total as f64
        }
    }
}

/// Per-run scratch space threaded into every stage call. `coverage`
/// is the structured metric block; `extras` is a free-form
/// JSON-shaped map for stages that need to stash intermediate state
/// (e.g. a `distill_run_id` set by the orchestrator and read by
/// finalize) without growing [`Coverage`].
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub coverage: Coverage,
    pub extras: JsonMap<String, JsonValue>,
}

impl Ctx {
    pub fn new() -> Self {
        Self::default()
    }
}
