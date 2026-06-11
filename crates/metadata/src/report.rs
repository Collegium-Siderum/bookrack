// SPDX-License-Identifier: Apache-2.0

//! Report types produced by [`crate::audit`].
//!
//! Every type here is plain data — the audit is a pure function and
//! consumers read the report rather than mutate it. The shapes are
//! shared between machine consumers (the CLI's `--json`, future MCP
//! tools) and human renderers (the CLI's default list), so each flag
//! and grade exists as a single enum variant rather than as a free-form
//! string.

use bookrack_catalog::EffectiveAttrs;
use bookrack_extract::{Biblio, Provenance};

/// How strong the evidence for a single field's value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldGrade {
    /// No effective value at all.
    Missing,
    /// Present but flagged for at least one weakness.
    Weak,
    /// Present and clean of weaknesses, but without an upgrading signal.
    Medium,
    /// Present, clean, and corroborated by an upgrading signal.
    Strong,
}

impl FieldGrade {
    /// A short token for the grade, suitable for logs and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            FieldGrade::Missing => "missing",
            FieldGrade::Weak => "weak",
            FieldGrade::Medium => "medium",
            FieldGrade::Strong => "strong",
        }
    }
}

/// The audit's binary "did anything required-field fire?" summary.
///
/// **This is a plausibility check, not a review status.** `Clean` means
/// the deterministic signals did not weaken any required field below
/// medium; it does *not* mean the metadata is correct, and the pipeline
/// never derives `node_reviews.status` from it. Review status is a
/// strictly human/LLM concern handled in `bookrack-catalog`; this enum
/// is the audit's own internal summary, used (for example) by
/// `--hold-for-metadata` to decide whether to park a book before
/// embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every required field is at [`FieldGrade::Medium`] or better.
    /// The record *looks* plausible — not verified.
    Clean,
    /// At least one required field is [`FieldGrade::Missing`] or
    /// [`FieldGrade::Weak`].
    NeedsWork,
}

impl Verdict {
    /// A short token for the verdict, suitable for logs and audit notes.
    /// **Not** a `node_reviews.status` value — see the enum docs.
    pub fn as_token(self) -> &'static str {
        match self {
            Verdict::Clean => "clean",
            Verdict::NeedsWork => "needs_work",
        }
    }
}

/// Row-level confidence rolled up from the per-field grades. Written
/// back into `node_publication_attrs.confidence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    /// The string token used in `node_publication_attrs.confidence`.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }
}

/// One structured weakness an audit signal raised against a field.
///
/// Variants are added rather than reused: a single flag matched on a
/// known token beats a string downstream consumers must parse. See
/// `signals.rs` for which signal raises which flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    /// The source format is a weak prior for bibliographic data
    /// (text/PDF/HTML).
    SourcePriorWeak,
    /// The extracted text layer was judged doubtful: every biblio field
    /// is downgraded one grade.
    DoubtfulTextLayer,
    /// The ISBN-10/13 checksum did not validate.
    IsbnCheckFailed,
    /// The year falls outside the plausible publication range.
    YearOutOfRange,
    /// The declared language disagrees with the body sample's script.
    LangMismatchesBody,
    /// The language tag is not a syntactically valid BCP-47 token.
    NonBcp47,
    /// The value looks like a watermark / distribution channel rather
    /// than a real publisher name.
    SourceWatermark,
    /// The publisher matched the curated whitelist of known reputable
    /// imprints.
    PublisherWhitelisted,
    /// The PDF `/Info` year is more likely the file's creation date
    /// than the work's publication year.
    PdfYearLikelyFileDate,
    /// The value is a placeholder (e.g. "Unknown", "Anonymous").
    PlaceholderValue,
    /// The value equals the bare source filename.
    EqualsFilename,
    /// A non-publisher field equals the publisher value.
    EqualsPublisher,
    /// The value is empty after trimming.
    Empty,
    /// The value is entirely numeric where it should not be.
    PurelyNumeric,
    /// The title carries a leading or trailing bracketed segment whose
    /// inner content looks like a series name (no sentence-end
    /// punctuation, no aggregator / volume marker shape).
    TitleSeriesParen,
    /// The title carries a bracketed marketing block: either lenticular
    /// brackets at the tail, or any bracket pair whose inner content
    /// contains sentence-end punctuation.
    TitleMarketingBlock,
    /// The title carries a leading aggregator marker — `[xxx]` or
    /// `\u{3010}xxx\u{3011}` at the head, typical of repackaged uploads.
    TitleAggregatorMarker,
    /// The title carries a volume / edition marker — bracketed content
    /// like `xxx\u{518C}` (volume), `xxx\u{7248}` (edition), or `Indexed`.
    /// Flagged for observability but does not weaken the grade.
    TitleVolumeMarker,
    /// The raw date string carried a time component (e.g. `T...:...`),
    /// which strongly suggests the year came from the file's
    /// production timestamp rather than a publication-date field.
    DateLooksLikeTimestamp,
    /// At least one TOC entry's `start_block` did not resolve. EPUB
    /// hrefs to missing spine documents and PDF outline targets past
    /// the last page both land here. HTML / TXT cannot trigger this.
    TocUnanchoredSome,
    /// More than half the TOC entries are unanchored, a stronger form
    /// of [`Flag::TocUnanchoredSome`] that promotes the shape to the
    /// `severe` severity band.
    TocUnanchoredHalf,
    /// The TOC has enough entries to plausibly express a hierarchy yet
    /// every entry sits at the same depth — likely a flattened or
    /// truncated nav.
    TocSuspiciousFlat,
    /// The TOC entry count and the body's heading-block count diverge
    /// by a wide ratio, suggesting the nav and the prose disagree on
    /// the document's structure.
    TocHeadingBlockSkew,
    /// The TOC is empty and the body has enough blocks for one to be
    /// expected. Treats a substantial book with no nav as a strong
    /// shape signal.
    TocEmptyLargeBody,
}

impl Flag {
    /// A short token for log lines and JSON output.
    pub fn token(self) -> &'static str {
        match self {
            Flag::SourcePriorWeak => "source_prior_weak",
            Flag::DoubtfulTextLayer => "doubtful_text_layer",
            Flag::IsbnCheckFailed => "isbn_check_failed",
            Flag::YearOutOfRange => "year_out_of_range",
            Flag::LangMismatchesBody => "lang_mismatches_body",
            Flag::NonBcp47 => "non_bcp47",
            Flag::SourceWatermark => "source_watermark",
            Flag::PublisherWhitelisted => "publisher_whitelisted",
            Flag::PdfYearLikelyFileDate => "pdf_year_likely_file_date",
            Flag::PlaceholderValue => "placeholder_value",
            Flag::EqualsFilename => "equals_filename",
            Flag::EqualsPublisher => "equals_publisher",
            Flag::Empty => "empty",
            Flag::PurelyNumeric => "purely_numeric",
            Flag::TitleSeriesParen => "title_series_paren",
            Flag::TitleMarketingBlock => "title_marketing_block",
            Flag::TitleAggregatorMarker => "title_aggregator_marker",
            Flag::TitleVolumeMarker => "title_volume_marker",
            Flag::DateLooksLikeTimestamp => "date_looks_like_timestamp",
            Flag::TocUnanchoredSome => "toc:unanchored_some",
            Flag::TocUnanchoredHalf => "toc:unanchored_half",
            Flag::TocSuspiciousFlat => "toc:suspicious_flat",
            Flag::TocHeadingBlockSkew => "toc:heading_block_skew",
            Flag::TocEmptyLargeBody => "toc:empty_large_body",
        }
    }
}

/// One field's audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldReport {
    /// The `node_publication_attrs` column name being audited.
    pub field: String,
    /// The aggregated grade.
    pub grade: FieldGrade,
    /// Every flag that fired against the field.
    pub flags: Vec<Flag>,
    /// One short human-facing line that summarises the row.
    pub hint: String,
}

/// The full audit output for one book root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataReport {
    /// Per-field rows, in stable display order.
    pub fields: Vec<FieldReport>,
    /// Aggregated verdict over the required fields.
    pub verdict: Verdict,
    /// Row-level confidence written back into
    /// `node_publication_attrs.confidence`.
    pub confidence: Confidence,
    /// Block indices that may contain a copyright page — candidates
    /// for a downstream cross-check, not asserted matches.
    pub copyright_blocks: Vec<usize>,
    /// TOC-shape flags from the warning-level shape audit. Held on a
    /// separate track from [`Self::fields`] so the seven publication-
    /// field histograms downstream consumers count are unchanged. The
    /// shape audit can only push [`Self::verdict`] toward `NeedsWork`
    /// and [`Self::confidence`] toward `Low`; it never strengthens
    /// either.
    pub shape_flags: Vec<Flag>,
}

/// Warning-level TOC shape statistics over one [`bookrack_extract::Extraction`].
///
/// Computed by the ingest STRUCTURE step (so the source's TOC, with
/// any unresolved or skewed entries, is described faithfully) and
/// passed back into [`AuditInput`] as a consultative input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TocStats {
    /// Total count of TOC entries the extractor produced.
    pub total_toc_entries: usize,
    /// TOC entries whose `start_block` could not be resolved.
    pub unanchored_toc_entries: usize,
    /// True when the TOC has enough entries to plausibly express a
    /// hierarchy yet every entry sits at the same depth.
    pub suspicious_flat: bool,
    /// True when TOC entry count and body heading-block count diverge
    /// badly.
    pub heading_block_skew: bool,
}

/// Everything one audit run needs, gathered by the caller.
///
/// The audit reads `effective` for the field values it grades (so a
/// post-hoc override flips the grade on the next run) and reads the
/// extracted `biblio` / `provenance` for signals that depend on the
/// raw extraction (source-format prior, text-layer quality, the
/// contributor list, PDF year heuristics).
pub struct AuditInput<'a> {
    /// The extracted bibliographic record. Mostly used for signals
    /// that need raw extraction context; values to grade are read from
    /// `effective`.
    pub biblio: &'a Biblio,
    /// The extractor's provenance, used to set the source-format prior.
    pub provenance: &'a Provenance,
    /// The effective field values (base + overrides) the audit grades.
    pub effective: &'a EffectiveAttrs,
    /// TOC shape statistics, consumed as a warning-level signal.
    pub toc_stats: &'a TocStats,
    /// Concatenated text of the book's first few blocks. Used by the
    /// language signal to compare the declared language against the
    /// body's script.
    pub body_sample: &'a str,
    /// Total block count in the source extraction, used to bound the
    /// copyright-page candidate range.
    pub total_blocks: usize,
    /// The source file's stem (no extension). Used to flag a title
    /// that merely echoes the filename.
    pub source_stem: Option<&'a str>,
    /// Runtime-loaded data set the audit consults — publisher
    /// whitelist, watermark patterns and tokens, placeholder titles,
    /// abbreviation expansions, and volume-suffix tokens. Pass
    /// [`crate::AuditData::empty()`] to disable every list.
    pub data: &'a crate::AuditData,
}
