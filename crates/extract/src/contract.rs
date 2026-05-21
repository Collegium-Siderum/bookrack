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

/// What extraction yielded for one source file — the deliverable of an
/// adapter.
///
/// A born-digital format (EPUB / HTML / TXT) always yields
/// [`ExtractOutcome::Extracted`]: it carries a structured text layer by
/// construction. A PDF may instead carry no usable text layer — a bare
/// scan, or a layer too corrupt to use — and then yields
/// [`ExtractOutcome::NeedsOcr`], routing the file onto the OCR path.
///
/// There is deliberately no third "cannot be handled at all" variant.
/// Such a state was considered and consciously deferred: the licence
/// and feasibility review judged every format in scope either
/// extractable or OCR-able, and no file in the corpus needs it. It
/// should be introduced only when a real format arrives that is
/// neither — not as empty scaffolding before then.
// The two variants differ greatly in size, but the large one is the
// overwhelmingly common result and an `ExtractOutcome` is produced and
// consumed one at a time, never held in bulk — boxing would only add an
// allocation to the hot path for no aggregate-memory gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ExtractOutcome {
    /// A usable text layer was extracted.
    Extracted(Extraction),
    /// No usable text layer — absent, too sparse, or corrupt. The file
    /// must be routed to OCR; `reason` records why, for the audit log.
    NeedsOcr { reason: String },
}

/// One source file, fully extracted.
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
    /// For EPUB, the spine-document index (the reader position); for
    /// PDF, the 0-based page index.
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

/// One source sub-unit that extraction skipped rather than aborting on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedUnit {
    /// 0-based index of the skipped sub-unit — for PDF, the page index.
    pub index: u32,
    /// Why it was skipped, for the audit log.
    pub reason: String,
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
    /// Sub-units skipped during extraction (a malformed PDF page, say)
    /// rather than aborting the whole file. Empty for born-digital
    /// formats, which abort on any sub-unit failure.
    pub skipped_units: Vec<SkippedUnit>,
}

/// The quality grade of an extracted text layer.
///
/// This enum describes only text that was *kept*: there is no
/// "needs OCR" grade. A file with no usable text layer never produces
/// an [`Extraction`] at all — it yields [`ExtractOutcome::NeedsOcr`] —
/// so by the time a `TextLayerQuality` is stamped, the text is already
/// known to be worth keeping; the grade only says how much to trust it.
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
}

/// Why extraction failed outright.
///
/// Each variant is a distinct *structural* failure the caller can react
/// to separately: the file as a whole cannot be opened or read, so the
/// whole book aborts. A failure confined to one sub-unit is not an
/// error — that sub-unit is skipped and recorded in
/// [`Provenance::skipped_units`], and extraction continues. Born-digital
/// formats record no skips: a missing spine document means the book is
/// genuinely broken, so they abort on any sub-unit failure.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// No adapter for this file's format.
    #[error("unsupported format: {detected}")]
    UnsupportedFormat { detected: String },
    /// The file could not be read, decompressed, or parsed as the
    /// container its extension claims — corrupt at the file level.
    #[error("corrupt file: {detail}")]
    CorruptFile { detail: String },
    /// The container opened but is structurally broken.
    #[error("malformed package: {detail}")]
    MalformedPackage { detail: String },
    /// The file is encrypted with a digital-rights-management scheme.
    /// This project does not decrypt: such a file is rejected outright
    /// rather than partially read.
    #[error("file is DRM-protected")]
    DrmProtected,
    /// Extraction produced zero body blocks — a real failure, since a
    /// book with no prose is not an empty success.
    #[error("extraction produced no body blocks")]
    EmptyExtraction,
    /// An underlying I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
