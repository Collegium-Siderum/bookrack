// SPDX-License-Identifier: Apache-2.0

//! The `extract` crate's product contract.
//!
//! A source file is extracted into one format-neutral [`Extraction`].
//! STRUCTURE consumes `blocks` and `toc`; METADATA consumes `biblio`;
//! the EXTRACT stage stamps `provenance`. The contract carries no
//! format-specific concept — no "EPUB", no "spine" — so every adapter,
//! whatever the source format, yields the same shape and STRUCTURE
//! faces a single input type.
//!
//! Everything in [`Extraction`] derives `PartialEq` so the determinism
//! invariant (same source file => byte-identical `Extraction`) reduces
//! to a plain `==` check.

use serde::Serialize;

/// One source file, fully extracted. The deliverable of an adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Extraction {
    /// Content blocks in reading order.
    pub blocks: Vec<Block>,
    /// The table-of-contents tree, flattened and depth-tagged.
    pub toc: Toc,
    /// Bibliographic metadata the file itself carries.
    pub biblio: Biblio,
    /// How the file was extracted, and with which extractor versions.
    pub provenance: Provenance,
}

/// A single content block — the cross-cutting unit STRUCTURE refines
/// into prose / structural leaves.
///
/// There is deliberately no source-anchor field. Anchor ids (a block's
/// own id, inline `<a>` ids) are extraction-internal scaffolding used
/// only to resolve TOC hrefs onto blocks; once [`TocEntry::start_block`]
/// is computed they serve no downstream purpose, and a single block can
/// carry several ids that one field could not represent anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Block {
    /// Coarse extraction-time classification.
    pub kind: BlockKind,
    /// The block's text, raw — whitespace from XML formatting is
    /// collapsed, but no NFKC / punctuation normalization is applied
    /// (that is STRUCTURE's job and carries its own version dimension).
    pub text: String,
    /// Which physical sub-unit of the source this block came from.
    /// For EPUB, the spine-document index (the reader position).
    pub source_unit: u32,
}

/// Coarse block classification. Deliberately fewer values than the
/// downstream `NodeType`s — extract labels only what the source can
/// give reliably, and STRUCTURE refines with whole-tree context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockKind {
    /// Running prose. The overwhelming majority.
    Body,
    /// A heading, with its source-given nesting depth (1 = topmost).
    Heading { level: u8 },
    /// A footnote / endnote body.
    Footnote,
    /// A caption attached to a figure or table.
    Caption,
    /// Recognized non-prose extract cannot place precisely.
    Other,
}

/// The table of contents: flattened, depth-tagged entries in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Toc {
    pub entries: Vec<TocEntry>,
}

/// One TOC entry, anchored to the block where its content begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TocEntry {
    pub label: String,
    /// 0 = topmost.
    pub depth: u8,
    /// Index into [`Extraction::blocks`] where this entry's content
    /// begins. `None` if the href could not be resolved.
    pub start_block: Option<usize>,
}

/// Bibliographic metadata transcribed faithfully from the file. Any
/// field may be absent for a bare file.
///
/// There is no full publication date here: a file's own metadata yields
/// at most a year, and extract transcribes only what the file carries.
/// Month/day precision is supplied later, by the METADATA stage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Biblio {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub publisher: Option<String>,
    pub year: Option<i32>,
    pub isbn: Option<String>,
    pub series: Option<String>,
    pub language: Option<String>,
    pub contributors: Vec<Contributor>,
}

/// One named contributor with the role the file assigned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contributor {
    pub name: String,
    pub role: ContributorRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributorRole {
    Author,
    Translator,
    Editor,
    /// Any other role, or an unspecified one.
    Other,
}

/// How the file was extracted, plus the boundary verdict on its text
/// layer (the extract / OCR seam).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Provenance {
    /// Which adapter produced this — `"epub"`, `"html"`, `"txt"`, …
    pub adapter: String,
    /// Behaviour-sensitive extractor versions, concatenated. A change
    /// here means block boundaries may shift: downstream re-extracts.
    pub extractor_version: String,
    /// The text-layer quality verdict.
    pub text_layer_quality: TextLayerQuality,
}

/// The extract / OCR boundary verdict for a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextLayerQuality {
    /// Structured, born-digital text (EPUB / HTML / TXT). Always usable.
    BornDigital,
    /// A text layer that quality checks judged usable (e.g. a good PDF).
    Usable,
    /// A text layer present but of doubtful quality — extracted, but the
    /// caller should treat it with low confidence.
    Doubtful,
    /// No usable text layer — the file should be routed to OCR instead.
    NeedsOcr,
}

/// Why extraction failed.
///
/// Each variant is a distinct failure the caller can react to
/// separately, rather than a single opaque error. Extraction fails
/// loudly: a broken sub-unit aborts the whole book rather than silently
/// dropping chapters.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// No adapter for this file's format.
    #[error("unsupported format: {detected}")]
    UnsupportedFormat { detected: String },
    /// The file could not be read or is not a valid archive.
    #[error("corrupt archive: {0}")]
    CorruptArchive(String),
    /// The container opened but is structurally broken.
    #[error("malformed package: {detail}")]
    MalformedPackage { detail: String },
    /// Extraction produced zero body blocks — a real failure, since a
    /// book with no prose is not an empty success.
    #[error("extraction produced no body blocks")]
    EmptyExtraction,
    /// An underlying I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
