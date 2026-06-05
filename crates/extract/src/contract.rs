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

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtractOutcome {
    /// A usable text layer was extracted.
    Extracted(Extraction),
    /// No usable text layer — absent, too sparse, or corrupt. The file
    /// must be routed to OCR; `reason` records why, for the audit log.
    NeedsOcr { reason: String },
}

/// One source file, fully extracted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toc {
    pub entries: Vec<TocEntry>,
}

/// One TOC entry, anchored to the block where its content begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Biblio {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub publisher: Option<String>,
    pub year: Option<i32>,
    /// The raw date string the adapter read, before any year extraction.
    /// EPUBs in particular often store a build-time `<dc:date>` whose
    /// shape (`YYYY-MM-DDThh:mm:ss...`) is a strong hint that the year
    /// is the file's production date rather than the work's publication
    /// year. The PDF adapter mirrors this with `/Info CreationDate` —
    /// the standard `D:YYYYMMDDHHmmSSOHH'mm'` shape that PDF writers
    /// stamp at export time. Kept verbatim so the audit can inspect it;
    /// absent when the adapter never saw a date string (HTML, TXT).
    pub year_raw: Option<String>,
    pub isbn: Option<String>,
    pub series: Option<String>,
    pub language: Option<String>,
    pub contributors: Vec<Contributor>,
}

/// One named contributor with the role the file assigned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contributor {
    pub name: String,
    pub role: ContributorRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributorRole {
    Author,
    Translator,
    Editor,
    /// Any other role, or an unspecified one.
    Other,
}

impl ContributorRole {
    /// The role's canonical string form, matching the `snake_case` serde
    /// representation and the convention `node_contributors.role` stores.
    /// Adding a variant without updating this match — and the test that
    /// pins it to the serde form — is a build-time error.
    pub const fn as_str(self) -> &'static str {
        match self {
            ContributorRole::Author => "author",
            ContributorRole::Translator => "translator",
            ContributorRole::Editor => "editor",
            ContributorRole::Other => "other",
        }
    }
}

/// One source sub-unit that extraction skipped rather than aborting on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedUnit {
    /// 0-based index of the skipped sub-unit — for PDF, the page index.
    pub index: u32,
    /// Why it was skipped, for the audit log.
    pub reason: String,
}

/// How the file was extracted, plus the boundary verdict on its text
/// layer (the extract / OCR seam).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Which adapter produced this — `"epub"`, `"html"`, `"txt"`, …
    pub adapter: String,
    /// Value of [`crate::EXTRACTOR_VERSION`] at the moment this file
    /// was extracted. A mismatch with the current const means block
    /// boundaries may have shifted: downstream re-extracts.
    pub extractor_version: u32,
    /// The text-layer quality verdict.
    pub text_layer_quality: TextLayerQuality,
    /// Sub-units skipped during extraction (a malformed PDF page, say)
    /// rather than aborting the whole file. Empty for born-digital
    /// formats, which abort on any sub-unit failure.
    pub skipped_units: Vec<SkippedUnit>,
    /// For a derived manifestation (e.g. an OCR intake whose source
    /// is a scan PDF), the SHA-256 of the source bytes the derivation
    /// was performed from. `None` for born-digital adapters and for
    /// any extraction whose source IS the manifestation. Kept as a
    /// forensic field so a future schema change can materialize it
    /// as an `intake_id` edge without information loss.
    #[serde(default)]
    pub derived_from_sha256: Option<String>,
    /// For a partial OCR ingest (the user opted into
    /// `--allow-partial`), the 1-based sheet numbers actually present
    /// in the OCR product, ascending. `None` means the full expected
    /// page range is present — the normal case for every adapter.
    #[serde(default)]
    pub partial_pages: Option<Vec<u32>>,
}

/// The quality grade of an extracted text layer.
///
/// This enum describes only text that was *kept*: there is no
/// "needs OCR" grade. A file with no usable text layer never produces
/// an [`Extraction`] at all — it yields [`ExtractOutcome::NeedsOcr`] —
/// so by the time a `TextLayerQuality` is stamped, the text is already
/// known to be worth keeping; the grade only says how much to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_extraction() -> Extraction {
        Extraction {
            blocks: vec![
                Block {
                    kind: BlockKind::Heading { level: 1 },
                    text: "Chapter One".into(),
                    source_unit: 0,
                },
                Block {
                    kind: BlockKind::Body,
                    text: "Some prose here.".into(),
                    source_unit: 0,
                },
                Block {
                    kind: BlockKind::Footnote,
                    text: "A footnote body.".into(),
                    source_unit: 1,
                },
            ],
            toc: Toc {
                entries: vec![TocEntry {
                    label: "Chapter One".into(),
                    depth: 0,
                    start_block: Some(0),
                }],
            },
            biblio: Biblio {
                title: Some("Sample".into()),
                subtitle: None,
                publisher: Some("Acme Press".into()),
                year: Some(2020),
                year_raw: Some("2020-01-15T00:00:00Z".into()),
                isbn: Some("978-0-00-000000-0".into()),
                series: None,
                language: Some("en".into()),
                contributors: vec![Contributor {
                    name: "A. Author".into(),
                    role: ContributorRole::Author,
                }],
            },
            provenance: Provenance {
                adapter: "epub".into(),
                extractor_version: 1,
                text_layer_quality: TextLayerQuality::BornDigital,
                skipped_units: vec![SkippedUnit {
                    index: 3,
                    reason: "empty spine document".into(),
                }],
                derived_from_sha256: None,
                partial_pages: None,
            },
        }
    }

    #[test]
    fn extraction_round_trips_through_serde_json() {
        let original = sample_extraction();
        let bytes = serde_json::to_vec(&original).expect("serialize");
        let parsed: Extraction = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn contributor_role_as_str_matches_serde_form() {
        for role in [
            ContributorRole::Author,
            ContributorRole::Translator,
            ContributorRole::Editor,
            ContributorRole::Other,
        ] {
            let json = serde_json::to_value(role).expect("serialize");
            let serde_form = json.as_str().expect("string variant");
            assert_eq!(
                role.as_str(),
                serde_form,
                "as_str() drifted from serde form for {role:?}",
            );
        }
    }
}
