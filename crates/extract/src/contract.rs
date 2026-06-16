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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExtractOutcome {
    /// A usable text layer was extracted.
    Extracted(Extraction),
    /// No usable text layer — absent, too sparse, or corrupt. The file
    /// must be routed to OCR; `reason` records why, for the audit log.
    NeedsOcr { reason: String },
}

/// One source file, fully extracted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Geometry summary the heading heuristics consume. Set by the PDF
    /// adapter; left absent by adapters whose source has no glyph
    /// geometry (TXT, EPUB, OCR) and by older envelopes that predate
    /// this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<BlockStyle>,
}

/// Per-block geometry summary the paper heading heuristics consume.
/// Carried inside [`Block::style`] as `Some(...)` by the PDF adapter
/// and `None` by every other source. The numbers are in page
/// coordinates (PDF points); they are not normalized across pages.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlockStyle {
    /// Median character font size across the block.
    pub font_size_median: f32,
    /// 90th-percentile character font size. Sits above the median when
    /// the block mixes small caps or a slightly larger lead character.
    pub font_size_p90: f32,
    /// True when over 50 % of the block's characters carry a bold font
    /// weight (PDF font weight ≥ 600, aggregated per character).
    pub is_bold_majority: bool,
    /// Physical line count.
    pub line_count: u32,
    /// Left coordinate of the block's first line.
    pub x0_first_line: f32,
    /// Vertical gap above the block, normalized by the page's median
    /// line height. 0 at the top of a page or across a page break.
    pub above_gap_ratio: f32,
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
    /// The paper's abstract. Emitted by the paper-side structuring
    /// pass only; book-side adapters (TXT, EPUB, HTML, Markdown,
    /// generic PDF) never carry this kind.
    Abstract,
    /// Recognized non-prose extract cannot place precisely.
    Other,
}

/// Where a paper's heading / caption coloring came from, recorded on
/// [`Provenance`] for observability over thin-structure samples. The
/// values are mutually exclusive:
///
/// - [`SourceOfStructure::Outline`] — every heading came from the PDF
///   `/Outline` map.
/// - [`SourceOfStructure::Heuristic`] — every heading came from the
///   text-pattern + geometry heuristic.
/// - [`SourceOfStructure::Mixed`] — both signals contributed.
/// - [`SourceOfStructure::None`] — no heading was identified at all
///   (the paper appears flat to the structuring pass).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceOfStructure {
    Outline,
    Heuristic,
    Mixed,
    None,
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
    /// Digital Object Identifier of the paper, as carried by the file's
    /// own metadata or surfaced by the glean IDENTIFY pass. Book ingest
    /// leaves this `None`.
    pub doi: Option<String>,
    /// arXiv identifier, normalized to one of the two CSL-friendly
    /// shapes — new-form `NNNN.NNNNN` or old-form `cat/NNNNNNN` —
    /// without the surrounding `arXiv:` prefix. Paper-side only.
    pub arxiv_id: Option<String>,
    /// Journal or magazine ISSN, paper-side only. Kept as a string so
    /// the dashed canonical form (`1234-5678`) round-trips verbatim.
    pub issn: Option<String>,
    /// Container title — journal, conference proceedings, or book series
    /// title — the holding work that contains the paper.
    pub container_title: Option<String>,
    /// Abstract body in full text. Populated by the glean IDENTIFY pass;
    /// book ingest never sets it.
    pub abstract_text: Option<String>,
    /// The CSL item type the paper claims. `None` for books and for
    /// papers whose type cannot be inferred without a CrossRef /
    /// OpenAlex enrichment.
    pub csl_type: Option<CslType>,
}

/// One named contributor with the role the file assigned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contributor {
    pub name: String,
    pub role: ContributorRole,
    /// Family component of the contributor's name, separated for CSL-JSON
    /// emission. `None` for the book pipeline (which only carries
    /// `name`) and for paper contributors whose name was not split.
    pub family: Option<String>,
    /// Given component of the contributor's name. See [`Self::family`].
    pub given: Option<String>,
    /// ORCID iD as `0000-0002-1825-0097`, when the source carries one.
    pub orcid: Option<String>,
}

/// CSL 1.0.2 item type, restricted to the variants the workspace
/// actually emits. The serde representation matches the CSL string
/// values verbatim (`"article-journal"`, `"paper-conference"`, …) so
/// the catalog round-trips through `csl_type TEXT` without an extra
/// mapping step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CslType {
    /// A journal article — the most common paper shape.
    ArticleJournal,
    /// A conference paper carried by a `Proceedings of ...` venue.
    PaperConference,
    /// A book; the canonical shape for the ingest pipeline.
    Book,
    /// A chapter within a book.
    Chapter,
    /// A thesis or dissertation.
    Thesis,
    /// A technical or working report.
    Report,
    /// A web page, when neither journal nor book applies.
    Webpage,
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
    /// Where the paper structuring pass (heading / caption coloring)
    /// sourced its signal — `outline`, `heuristic`, `mixed`, or
    /// `none`. Absent on book-side extractions, which do not run that
    /// pass, and on older envelopes that predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_of_structure: Option<SourceOfStructure>,
    /// Silent-fallback events the adapter took during extraction:
    /// lossy decode, oversize-window truncation, malformed metadata
    /// strings the adapter parsed anyway, and similar paths where the
    /// adapter substituted a best-effort result rather than aborting.
    /// Each entry names a stable, namespaced kind from
    /// [`fallback_kinds`]; older envelopes that predate the field
    /// deserialize as an empty vector.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<FallbackEvent>,
}

/// One silent-fallback event recorded during extraction.
///
/// "Silent" here means: the adapter took the fallback path without
/// aborting and without surfacing it as a `Skipped` sub-unit. The
/// envelope keeps these so a downstream consumer can attribute a
/// surprising value to the path that produced it, and so aggregate
/// statistics over a library can show which fallback paths fire and
/// how often.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackEvent {
    /// Stable, namespaced identifier of the fallback. Adapters write
    /// the `&'static str` constants under [`fallback_kinds`] through
    /// [`FallbackEvent::record`]; the field is owned `String` so a
    /// roundtrip through a stored envelope deserializes into bytes
    /// the reader owns.
    pub kind: String,
    /// Optional free-form detail — e.g. the strict-decode error
    /// message that triggered a permissive fallback. Omitted when
    /// the kind alone is sufficient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl FallbackEvent {
    /// Push one event and emit the paired `tracing::warn!` so the
    /// envelope record and the live log line can never drift apart.
    /// `adapter` flows into the tracing event only; the envelope
    /// already carries the adapter on [`Provenance::adapter`]. The
    /// `kind` argument is `&'static str` so a caller has to reach for
    /// one of the [`fallback_kinds`] constants rather than inline a
    /// literal at the push site.
    pub fn record(
        events: &mut Vec<FallbackEvent>,
        adapter: &str,
        kind: &'static str,
        detail: Option<String>,
    ) {
        let detail_view: Option<&str> = detail.as_deref();
        tracing::warn!(
            event = "extract.fallback",
            adapter = adapter,
            kind = kind,
            detail = detail_view,
        );
        events.push(FallbackEvent {
            kind: kind.to_string(),
            detail,
        });
    }
}

/// Namespaced identifiers for [`FallbackEvent::kind`].
///
/// One constant per known silent-fallback path; adapters reference
/// the constant rather than inline a literal so the canonical set is
/// discoverable and a misspelled kind cannot ship.
pub mod fallback_kinds {
    /// TXT decoder: BOM-stripped UTF-8 path saw byte sequences that
    /// `from_utf8_lossy` replaced with U+FFFD — the file was not
    /// fully valid UTF-8 despite the BOM.
    pub const TXT_UTF8_LOSSY_SUBSTITUTION: &str = "txt:utf8_lossy_substitution";
    /// TXT decoder: strict UTF-8 trial failed; the adapter fell
    /// through to a permissive GB18030 decode. The detail string
    /// carries the `str::Utf8Error` that triggered the fall-through.
    pub const TXT_GB18030: &str = "txt:gb18030";
    /// HTML adapter: bytes were not valid UTF-8 and were decoded
    /// through `String::from_utf8_lossy`, substituting U+FFFD for
    /// every invalid sub-sequence.
    pub const HTML_UTF8_LOSSY: &str = "html:utf8_lossy";
    /// HTML adapter: the bounded 256 KiB head-scan window did not
    /// contain `</head>`, so any `<head>` metadata past the window
    /// was not consulted. Fires whenever the window was used without
    /// terminating in a real `</head>`.
    pub const HTML_HEAD_TRUNCATED_256K: &str = "html:head_truncated_256k";
    /// PDF book adapter: `/Info CreationDate` was present but did
    /// not carry the PDF-spec `D:` prefix; the adapter took the
    /// digit prefix anyway.
    pub const PDF_INFO_CREATION_DATE_NO_D_PREFIX: &str = "pdf:info_creation_date_no_d_prefix";
    /// EPUB adapter: a nav entry's `depth()` was 0, so
    /// `saturating_sub(1)` clamped at 0 rather than yielding a
    /// negative depth. Indicates a TOC entry the toolkit reported
    /// without descending below the (omitted) root.
    pub const EPUB_NAV_DEPTH_SATURATE: &str = "epub:nav_depth_saturate";
    /// EPUB adapter: `as_isbn` accepted an identifier value that
    /// contained `isbn` somewhere in the string but did not carry
    /// the canonical `urn:isbn:` prefix.
    pub const EPUB_ISBN_SUBSTRING_FALLBACK: &str = "epub:isbn_substring_fallback";
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
    #[error("I/O error")]
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
                    style: None,
                },
                Block {
                    kind: BlockKind::Body,
                    text: "Some prose here.".into(),
                    source_unit: 0,
                    style: None,
                },
                Block {
                    kind: BlockKind::Footnote,
                    text: "A footnote body.".into(),
                    source_unit: 1,
                    style: None,
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
                    family: None,
                    given: None,
                    orcid: None,
                }],
                ..Biblio::default()
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
                source_of_structure: None,
                fallbacks: Vec::new(),
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
