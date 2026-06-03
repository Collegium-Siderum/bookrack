//! Simulate an ingest up to (but not including) embedding.
//!
//! The dryrun drives the same pre-vector code path [`crate::ingest_book`]
//! uses — `extract` → register → `ingest_structure` → metadata audit →
//! `plan_book_chunks` — against an in-memory catalog and corpus that are
//! discarded when the run ends. The product is a [`DryrunBookReport`] per
//! source file plus a [`DryrunSummary`] over the set, both serialisable
//! to JSON.
//!
//! Embedding is the one stage that costs minutes per book and hits an
//! external service. Every step before it runs in milliseconds and is a
//! pure-CPU function of the source plus the catalog row, so the dryrun
//! reproduces the metadata audit's verdict exactly without paying that
//! cost. It is the read-only equivalent of an ingest: a tool to ask
//! "what would happen?" before committing to the real run.

use std::path::{Path, PathBuf};
use std::time::Instant;

use bookrack_catalog::{Catalog, NewIntake, NewReview};
use bookrack_core::NodeType;
use bookrack_corpus::Corpus;
use bookrack_extract::{Biblio, ExtractOutcome, Extraction, extract};
use bookrack_metadata::{AuditInput, FilenameBiblio, MetadataReport, audit, parse_filename};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    BOOK_SCOPE, ChunkParams, IngestError, METADATA_BODY_SAMPLE_BLOCKS, METADATA_BODY_SAMPLE_CHARS,
    StructureParams, body_sample, build_base_attrs, ingest_structure, plan_book_chunks,
};

/// Extensions the dryrun walker picks up under a directory.
pub const SUPPORTED_EXTENSIONS: &[&str] =
    &["epub", "pdf", "txt", "html", "htm", "mobi", "azw3", "djvu"];

/// Knobs for one dryrun.
#[derive(Debug, Clone, Default)]
pub struct DryrunParams {
    /// STRUCTURE tuning.
    pub structure: StructureParams,
    /// CHUNK tuning.
    pub chunk: ChunkParams,
    /// When true, the CHUNK step is skipped and `chunks` stays `None` in
    /// every book report. Useful if a caller only needs the audit verdict.
    pub skip_chunks: bool,
    /// Runtime-loaded rule set the metadata audit consults. Defaults
    /// to an empty set; load real rules from
    /// `Config::audit_rules_dir()` and assign.
    pub audit_rules: bookrack_metadata::AuditRules,
}

/// One book's dryrun outcome.
///
/// Every payload field is optional so a record can describe a successful
/// run, a NeedsOcr route, an unsupported format, or any extraction error
/// without changing shape.
#[derive(Debug, Clone, Serialize)]
pub struct DryrunBookReport {
    /// The file's stem, identifying it within the report.
    pub stem: String,
    /// Lowercased extension as the format key.
    pub format: String,
    /// File size in bytes.
    pub bytes: u64,
    /// `extracted` / `needs_ocr` / `unsupported` / `error`.
    pub extract_outcome: String,
    /// Adapter the extract layer reported (when extracted).
    pub adapter: Option<String>,
    /// Total `Extraction::blocks.len()`.
    pub blocks: Option<usize>,
    /// TOC shape statistics computed by STRUCTURE.
    pub toc_stats: Option<TocStatsOut>,
    /// How many corpus nodes / leaves STRUCTURE produced.
    pub structure: Option<StructureOut>,
    /// CHUNK plan statistics. `None` if [`DryrunParams::skip_chunks`].
    pub chunks: Option<ChunkStatsOut>,
    /// The base-attrs `source` tag chosen for this record.
    pub source_tag: Option<String>,
    /// Whether a Calibre-style filename template matched the stem.
    pub filename_template: Option<String>,
    /// Extracted biblio fields, mirrored for quick comparison.
    pub biblio: Option<BiblioOut>,
    /// Per-field values the filename parser recovered, separate from the
    /// label in [`Self::filename_template`]. `None` when no template
    /// matched; otherwise carries the parsed fields, any of which may be
    /// `None` on their own.
    pub filename_biblio: Option<FilenameBiblioOut>,
    /// The base-layer attributes [`build_base_attrs`] assembled from the
    /// extraction and the filename fallback, just before they are
    /// written to `node_publication_attrs`.
    pub base_attrs: Option<BaseAttrsOut>,
    /// Program-level overrides applied while assembling the base
    /// attrs — `drop_invalid_isbn`, `drop_stale_year`, and per-field
    /// `filename_fallback:<field>`. Empty when nothing was touched.
    pub base_attrs_actions: Vec<String>,
    /// The merged base-plus-overrides view the audit consumed. Carries
    /// every field the effective view holds, alongside any override rows
    /// that displaced a base value. In the in-memory dryrun catalog
    /// `overrides_applied` is always empty; a future `--from-catalog`
    /// path would seed it from real `node_overrides` rows.
    pub effective: Option<EffectiveOut>,
    /// Fields the volume→set inheritance rule (Q2-4.3) carried down from
    /// a parent node. `None` while the dryrun does not walk a multi-volume
    /// chain; reserved for the inherit-from path that lands with the
    /// multi-volume metadata feature.
    pub inherited_from_parent: Option<Vec<String>>,
    /// Per-field grades and flags from the metadata audit.
    pub audit_fields: Vec<FieldOut>,
    /// Shape-level flag tokens the audit raised against the TOC. Held
    /// separately from [`Self::audit_fields`] so the seven publication-
    /// field histograms downstream consumers count are unchanged. Each
    /// token carries a `toc:` prefix.
    pub audit_shape_flags: Vec<String>,
    /// Aggregated audit verdict (`clean` / `needs_work`).
    pub verdict: Option<String>,
    /// Row-level confidence (`high` / `medium` / `low`).
    pub confidence: Option<String>,
    /// Wall time spent inside the dryrun, in milliseconds.
    pub elapsed_ms: u64,
    /// Carried only when extract returned an error.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StructureOut {
    pub nodes_written: usize,
    pub prose_leaves: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TocStatsOut {
    pub total_toc_entries: usize,
    pub unanchored_toc_entries: usize,
    pub suspicious_flat: bool,
    pub heading_block_skew: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkStatsOut {
    pub count: usize,
    pub total_chars: usize,
    pub min_chars: usize,
    pub max_chars: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BiblioOut {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub publisher: Option<String>,
    pub year: Option<String>,
    pub isbn: Option<String>,
    pub language: Option<String>,
    pub series: Option<String>,
}

/// Fields the filename parser recovered for one book. Every field is
/// optional: a template match leaves the report `Some(..)` even when only
/// part of the shape filled in.
#[derive(Debug, Clone, Serialize)]
pub struct FilenameBiblioOut {
    pub title: Option<String>,
    pub author: Option<String>,
    pub year: Option<String>,
    pub publisher: Option<String>,
    pub isbn: Option<String>,
    pub series: Option<String>,
}

/// The base-layer attributes the dryrun would have written to
/// `node_publication_attrs`, surfaced for visibility into how the
/// extracted biblio plus the filename fallback combined.
#[derive(Debug, Clone, Serialize)]
pub struct BaseAttrsOut {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub publisher: Option<String>,
    pub year: Option<String>,
    pub publication_date: Option<String>,
    pub isbn: Option<String>,
    pub series: Option<String>,
    pub series_number: Option<String>,
    pub edition: Option<String>,
    pub language: Option<String>,
    pub original_title: Option<String>,
    pub original_language: Option<String>,
    pub source_format: Option<String>,
    pub source: Option<String>,
}

/// The effective view a downstream consumer would see, surfaced for
/// visibility so a JSONL diff can distinguish base values from
/// override values.
#[derive(Debug, Clone, Serialize)]
pub struct EffectiveOut {
    /// Effective field values keyed by field name. Mirrors what
    /// `EffectiveAttrs::iter` exposes.
    pub fields: std::collections::BTreeMap<String, String>,
    /// Each override that displaced a base value for a field. Empty in
    /// the in-memory dryrun catalog; reserved for a future
    /// `--from-catalog` path.
    pub overrides_applied: Vec<OverrideApplied>,
}

/// One override the effective view carried over the base layer.
#[derive(Debug, Clone, Serialize)]
pub struct OverrideApplied {
    pub field: String,
    pub base_value: Option<String>,
    pub override_value: Option<String>,
}

impl From<&bookrack_catalog::NewPublicationAttrs> for BaseAttrsOut {
    fn from(a: &bookrack_catalog::NewPublicationAttrs) -> BaseAttrsOut {
        BaseAttrsOut {
            title: a.title.clone(),
            subtitle: a.subtitle.clone(),
            publisher: a.publisher.clone(),
            year: a.year.clone(),
            publication_date: a.publication_date.clone(),
            isbn: a.isbn.clone(),
            series: a.series.clone(),
            series_number: a.series_number.clone(),
            edition: a.edition.clone(),
            language: a.language.clone(),
            original_title: a.original_title.clone(),
            original_language: a.original_language.clone(),
            source_format: a.source_format.clone(),
            source: a.source.clone(),
        }
    }
}

impl From<&FilenameBiblio> for FilenameBiblioOut {
    fn from(b: &FilenameBiblio) -> FilenameBiblioOut {
        FilenameBiblioOut {
            title: b.title.clone(),
            author: b.author.clone(),
            year: b.year.clone(),
            publisher: b.publisher.clone(),
            isbn: b.isbn.clone(),
            series: b.series.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldOut {
    pub field: String,
    pub grade: String,
    pub flags: Vec<String>,
}

/// The aggregate over a set of [`DryrunBookReport`]s.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DryrunSummary {
    /// Total files considered.
    pub n_files: usize,
    /// Files for each lowercased extension.
    pub formats: std::collections::BTreeMap<String, usize>,
    /// `extracted` / `needs_ocr` / `unsupported` / `error` counts.
    pub extract_outcomes: std::collections::BTreeMap<String, usize>,
    /// Verdict histogram over the books that produced an audit.
    pub verdicts: std::collections::BTreeMap<String, usize>,
    /// Confidence histogram over the books that produced an audit.
    pub confidence: std::collections::BTreeMap<String, usize>,
    /// Per-field grade histogram (field → grade → count).
    pub field_grades: std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>>,
    /// Per-field flag histogram (field → flag → count).
    pub flag_counts: std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>>,
    /// Base-attrs action histogram (action token → count). Sourced from
    /// [`DryrunBookReport::base_attrs_actions`]; the keys are stable
    /// tokens like `drop_invalid_isbn`, `drop_stale_year`, and
    /// `filename_fallback:<field>`.
    pub base_attrs_action_counts: std::collections::BTreeMap<String, usize>,
    /// TOC anomaly histogram (shape-flag token -> count). Sourced from
    /// [`DryrunBookReport::audit_shape_flags`]; keys carry a `toc:`
    /// prefix and stay on a separate axis from the per-field flag
    /// histogram in [`Self::flag_counts`].
    pub toc_anomaly_counts: std::collections::BTreeMap<String, usize>,
}

/// Walk a path, dryrun every supported file under it, and accumulate a
/// summary. `path` may be a single file or a directory.
pub fn dryrun_path(path: &Path, params: &DryrunParams) -> Vec<DryrunBookReport> {
    let files = collect_files(path);
    files.iter().map(|p| dryrun_book(p, params)).collect()
}

/// Dryrun one source file. Never panics: extract failures and structural
/// errors are recorded into the returned report rather than propagated.
pub fn dryrun_book(path: &Path, params: &DryrunParams) -> DryrunBookReport {
    let started = Instant::now();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let bytes = path.metadata().map(|m| m.len()).unwrap_or(0);

    let mut record = DryrunBookReport {
        stem: stem.clone(),
        format,
        bytes,
        extract_outcome: "error".to_string(),
        adapter: None,
        blocks: None,
        toc_stats: None,
        structure: None,
        chunks: None,
        source_tag: None,
        filename_template: None,
        biblio: None,
        filename_biblio: None,
        base_attrs: None,
        base_attrs_actions: vec![],
        effective: None,
        inherited_from_parent: None,
        audit_fields: vec![],
        audit_shape_flags: vec![],
        verdict: None,
        confidence: None,
        elapsed_ms: 0,
        error: None,
    };

    let extraction = match extract(path) {
        Ok(ExtractOutcome::Extracted(e)) => e,
        Ok(ExtractOutcome::NeedsOcr { reason }) => {
            record.extract_outcome = "needs_ocr".to_string();
            record.error = Some(reason);
            record.elapsed_ms = started.elapsed().as_millis() as u64;
            return record;
        }
        Err(e) => {
            // Unsupported formats are not a per-file failure — they are
            // expected for the formats this build has no adapter for.
            let message = format!("{e}");
            if matches!(e, bookrack_extract::ExtractError::UnsupportedFormat { .. }) {
                record.extract_outcome = "unsupported".to_string();
            } else {
                record.extract_outcome = "error".to_string();
            }
            record.error = Some(message);
            record.elapsed_ms = started.elapsed().as_millis() as u64;
            return record;
        }
    };

    // Past this point the extract succeeded. Any failure now is a bug,
    // not user data — surface it as an error rather than panicking.
    match run_pipeline(&extraction, &stem, path, params, &mut record) {
        Ok(()) => record.extract_outcome = "extracted".to_string(),
        Err(e) => {
            record.extract_outcome = "error".to_string();
            record.error = Some(format!("{e}"));
        }
    }
    record.elapsed_ms = started.elapsed().as_millis() as u64;
    record
}

/// Drive the pre-vector pipeline against fresh in-memory databases and
/// fill `record` with what each step produced. Separated from
/// [`dryrun_book`] so the success path can use `?` without sacrificing the
/// caller's "never returns an Err" guarantee.
fn run_pipeline(
    extraction: &Extraction,
    stem: &str,
    path: &Path,
    params: &DryrunParams,
    record: &mut DryrunBookReport,
) -> Result<(), IngestError> {
    record.adapter = Some(extraction.provenance.adapter.clone());
    record.blocks = Some(extraction.blocks.len());

    let mut catalog = Catalog::open_in_memory()?;
    let mut corpus = Corpus::open_in_memory()?;
    let registration = catalog.register_intake(
        &NewIntake::new(synthetic_sha(stem))
            .format(extraction.provenance.adapter.clone())
            .byte_size(extraction.blocks.iter().map(|b| b.text.len() as i64).sum())
            .original_path(path.to_string_lossy().into_owned()),
    )?;
    let intake_id = registration.intake().intake_id;
    catalog.set_extraction(
        intake_id,
        &extraction.provenance.adapter,
        &extraction.provenance.extractor_version,
    )?;

    // STRUCTURE.
    let structure_report = ingest_structure(
        &mut corpus,
        intake_id,
        NodeType::Work,
        extraction,
        &params.structure,
    )?;
    record.structure = Some(StructureOut {
        nodes_written: structure_report.nodes_written,
        prose_leaves: structure_report.prose_leaves,
    });
    let toc = structure_report.toc_stats;
    record.toc_stats = Some(TocStatsOut {
        total_toc_entries: toc.total_toc_entries,
        unanchored_toc_entries: toc.unanchored_toc_entries,
        suspicious_flat: toc.suspicious_flat,
        heading_block_skew: toc.heading_block_skew,
    });

    // METADATA — seed base attrs, then audit over the effective view.
    let filename_biblio = parse_filename(stem);
    record.filename_template = filename_template_label(&filename_biblio);
    record.filename_biblio = record
        .filename_template
        .is_some()
        .then(|| FilenameBiblioOut::from(&filename_biblio));
    let outcome = build_base_attrs(intake_id, extraction, Some(&filename_biblio));
    let mut attrs = outcome.attrs;
    record.source_tag = attrs.source.clone();
    record.base_attrs = Some(BaseAttrsOut::from(&attrs));
    record.base_attrs_actions = outcome.actions.iter().map(|a| a.token()).collect();
    catalog.upsert_publication_attrs(&attrs)?;
    let effective = catalog.effective_publication_attrs(intake_id, BOOK_SCOPE)?;
    record.effective = Some(EffectiveOut {
        fields: effective
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        overrides_applied: Vec::new(),
    });
    let body = body_sample(extraction);
    let report = audit(
        &AuditInput {
            biblio: &extraction.biblio,
            provenance: &extraction.provenance,
            effective: &effective,
            toc_stats: &structure_report.toc_stats,
            body_sample: &body,
            total_blocks: extraction.blocks.len(),
            source_stem: Some(stem),
            rules: &params.audit_rules,
        },
        &bookrack_metadata::AuditProfile::default(),
    );
    // Re-write the row with the confidence rollup, like ingest does, so
    // any downstream caller that opens the in-memory catalog sees the
    // post-audit value.
    attrs.confidence = Some(report.confidence.as_str().to_string());
    catalog.upsert_publication_attrs(&attrs)?;
    let _ = catalog.upsert_review(
        &NewReview::new(
            intake_id,
            BOOK_SCOPE,
            "pipeline",
            bookrack_catalog::STATUS_PENDING,
        )
        .notes(String::new()),
    );

    record.biblio = Some(biblio_out(&extraction.biblio));
    record.audit_fields = report_fields(&report);
    record.audit_shape_flags = report
        .shape_flags
        .iter()
        .map(|f| f.token().to_string())
        .collect();
    record.verdict = Some(report.verdict.as_token().to_string());
    record.confidence = Some(report.confidence.as_str().to_string());

    // CHUNK — the last pre-vector step. Skipped on request.
    if !params.skip_chunks {
        let plans = plan_book_chunks(&corpus, structure_report.book_root_id, &params.chunk)?;
        record.chunks = Some(chunk_stats(&plans));
    }

    Ok(())
}

/// Recursively collect every file under `path` whose extension matches one
/// of [`SUPPORTED_EXTENSIONS`]. A single-file `path` returns that file
/// when the extension matches and an empty list otherwise.
pub fn collect_files(path: &Path) -> Vec<PathBuf> {
    fn matches(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
            .unwrap_or(false)
    }
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                visit(&p, out);
            } else if matches(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    if path.is_dir() {
        visit(path, &mut out);
        out.sort();
    } else if matches(path) {
        out.push(path.to_path_buf());
    }
    out
}

/// Aggregate over a slice of book reports. Pure: walks the field grades
/// and flags and counts them. Useful both to embed a summary in the run
/// artifact and to print a stderr line.
pub fn summarize(books: &[DryrunBookReport]) -> DryrunSummary {
    let mut summary = DryrunSummary {
        n_files: books.len(),
        ..DryrunSummary::default()
    };
    for book in books {
        *summary.formats.entry(book.format.clone()).or_default() += 1;
        *summary
            .extract_outcomes
            .entry(book.extract_outcome.clone())
            .or_default() += 1;
        if let Some(v) = &book.verdict {
            *summary.verdicts.entry(v.clone()).or_default() += 1;
        }
        if let Some(c) = &book.confidence {
            *summary.confidence.entry(c.clone()).or_default() += 1;
        }
        for field in &book.audit_fields {
            *summary
                .field_grades
                .entry(field.field.clone())
                .or_default()
                .entry(field.grade.clone())
                .or_default() += 1;
            for flag in &field.flags {
                *summary
                    .flag_counts
                    .entry(field.field.clone())
                    .or_default()
                    .entry(flag.clone())
                    .or_default() += 1;
            }
        }
        for action in &book.base_attrs_actions {
            *summary
                .base_attrs_action_counts
                .entry(action.clone())
                .or_default() += 1;
        }
        for token in &book.audit_shape_flags {
            *summary.toc_anomaly_counts.entry(token.clone()).or_default() += 1;
        }
    }
    summary
}

fn synthetic_sha(stem: &str) -> String {
    let mut h = Sha256::new();
    h.update(stem.as_bytes());
    format!("{:x}", h.finalize())
}

fn biblio_out(b: &Biblio) -> BiblioOut {
    BiblioOut {
        title: b.title.clone(),
        subtitle: b.subtitle.clone(),
        publisher: b.publisher.clone(),
        year: b.year.map(|y| y.to_string()),
        isbn: b.isbn.clone(),
        language: b.language.clone(),
        series: b.series.clone(),
    }
}

fn filename_template_label(fb: &FilenameBiblio) -> Option<String> {
    let anything = fb.title.is_some()
        || fb.author.is_some()
        || fb.year.is_some()
        || fb.publisher.is_some()
        || fb.isbn.is_some();
    anything.then(|| "calibre".to_string())
}

fn report_fields(report: &MetadataReport) -> Vec<FieldOut> {
    report
        .fields
        .iter()
        .map(|f| FieldOut {
            field: f.field.clone(),
            grade: format!("{:?}", f.grade).to_ascii_lowercase(),
            flags: f.flags.iter().map(|fl| fl.token().to_string()).collect(),
        })
        .collect()
}

fn chunk_stats(plans: &[crate::ChunkPlan]) -> ChunkStatsOut {
    if plans.is_empty() {
        return ChunkStatsOut {
            count: 0,
            total_chars: 0,
            min_chars: 0,
            max_chars: 0,
        };
    }
    let mut total = 0usize;
    let mut min = usize::MAX;
    let mut max = 0usize;
    for p in plans {
        let n = p.text.chars().count();
        total += n;
        min = min.min(n);
        max = max.max(n);
    }
    ChunkStatsOut {
        count: plans.len(),
        total_chars: total,
        min_chars: min,
        max_chars: max,
    }
}

// Silence the unused-constant warning when callers use this crate without
// reaching for these. They are referenced from the imports for clarity,
// but the dryrun consumes them through `body_sample` already.
const _: usize = METADATA_BODY_SAMPLE_BLOCKS + METADATA_BODY_SAMPLE_CHARS;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_html(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = fs::File::create(&path).expect("create");
        writeln!(
            f,
            "<!doctype html><html><head><title>A Title</title>\
             <meta name=\"author\" content=\"An Author\"></head>\
             <body><h1>Heading</h1><p>{body}</p></body></html>"
        )
        .expect("write");
        path
    }

    #[test]
    fn dryrun_on_a_synthetic_html_produces_a_full_report() {
        let dir = tempdir().expect("tempdir");
        let path = write_html(
            dir.path(),
            "tiny.html",
            "The first paragraph of a tiny synthetic book that has just \
             enough text to chunk into a single window after grouping.",
        );
        let rec = dryrun_book(&path, &DryrunParams::default());
        assert_eq!(rec.extract_outcome, "extracted");
        assert_eq!(rec.format, "html");
        assert!(rec.blocks.unwrap_or(0) >= 1);
        assert!(rec.structure.is_some());
        // The chunk planner runs unless explicitly skipped.
        let chunks = rec.chunks.expect("chunks ran");
        assert!(chunks.count >= 1, "{chunks:?}");
        // The metadata audit produces some field rows even on a minimal file.
        assert!(!rec.audit_fields.is_empty());
        assert!(rec.verdict.is_some());
        assert!(rec.confidence.is_some());
        // A tiny HTML fixture cannot raise a TOC shape signal: HTML
        // headings are anchored by construction and the body is too
        // short to trip the large-body bit.
        assert!(rec.audit_shape_flags.is_empty());
    }

    #[test]
    fn a_calibre_template_filename_populates_the_filename_biblio_layer() {
        let dir = tempdir().expect("tempdir");
        let path = write_html(
            dir.path(),
            "An Author - A Title (2020, A Publisher).html",
            "Body.",
        );
        let rec = dryrun_book(&path, &DryrunParams::default());
        assert_eq!(rec.extract_outcome, "extracted");
        assert_eq!(rec.filename_template.as_deref(), Some("calibre"));
        let fb = rec
            .filename_biblio
            .expect("filename_biblio populated when the filename template matched");
        assert_eq!(fb.title.as_deref(), Some("A Title"));
        assert_eq!(fb.author.as_deref(), Some("An Author"));
        assert_eq!(fb.year.as_deref(), Some("2020"));
        assert_eq!(fb.publisher.as_deref(), Some("A Publisher"));
        assert_eq!(fb.isbn, None);
        assert_eq!(fb.series, None);
    }

    #[test]
    fn a_non_calibre_filename_leaves_the_filename_biblio_layer_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_html(dir.path(), "tiny.html", "Body.");
        let rec = dryrun_book(&path, &DryrunParams::default());
        assert_eq!(rec.extract_outcome, "extracted");
        assert!(rec.filename_template.is_none());
        assert!(rec.filename_biblio.is_none());
    }

    #[test]
    fn the_effective_layer_mirrors_the_view_the_audit_consumed() {
        let dir = tempdir().expect("tempdir");
        let path = write_html(dir.path(), "tiny.html", "Body.");
        let rec = dryrun_book(&path, &DryrunParams::default());
        let effective = rec.effective.expect("effective populated on success");
        // The HTML adapter surfaces `<title>` as the title field, so the
        // effective view carries at least that. The in-memory catalog
        // has no overrides, so the trail stays empty.
        assert_eq!(
            effective.fields.get("title").map(String::as_str),
            Some("A Title")
        );
        assert!(effective.overrides_applied.is_empty());
        assert!(rec.inherited_from_parent.is_none());
    }

    #[test]
    fn dryrun_skips_chunking_on_request() {
        let dir = tempdir().expect("tempdir");
        let path = write_html(dir.path(), "tiny.html", "Some prose body.");
        let params = DryrunParams {
            skip_chunks: true,
            ..Default::default()
        };
        let rec = dryrun_book(&path, &params);
        assert_eq!(rec.extract_outcome, "extracted");
        assert!(rec.chunks.is_none());
        // The metadata audit still runs.
        assert!(rec.verdict.is_some());
    }

    #[test]
    fn dryrun_records_unsupported_format() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("a.mobi");
        fs::write(&path, b"not really a mobi").expect("write");
        let rec = dryrun_book(&path, &DryrunParams::default());
        assert_eq!(rec.extract_outcome, "unsupported");
        assert!(rec.error.as_deref().unwrap_or("").contains("mobi"));
        assert!(rec.verdict.is_none());
    }

    #[test]
    fn summarize_counts_verdicts_and_grades() {
        let dir = tempdir().expect("tempdir");
        let a = write_html(dir.path(), "a.html", "Paragraph one.");
        let b = write_html(dir.path(), "b.html", "Paragraph two.");
        let reports = vec![
            dryrun_book(&a, &DryrunParams::default()),
            dryrun_book(&b, &DryrunParams::default()),
        ];
        let summary = summarize(&reports);
        assert_eq!(summary.n_files, 2);
        assert_eq!(summary.formats.get("html").copied(), Some(2));
        assert_eq!(summary.extract_outcomes.get("extracted").copied(), Some(2));
        assert!(summary.verdicts.values().sum::<usize>() == 2);
        // At least the title field shows up across both books.
        let title_grades = summary.field_grades.get("title").expect("title grades");
        assert_eq!(title_grades.values().sum::<usize>(), 2);
        // The histogram exists, even if both books happened to record zero
        // base-attrs actions on this fixture.
        let _ = summary.base_attrs_action_counts;
        // Two clean synthetic HTML books cannot raise TOC anomaly
        // tokens, so the histogram stays empty here.
        assert!(summary.toc_anomaly_counts.is_empty());
    }

    #[test]
    fn summarize_counts_toc_anomalies() {
        // The synthetic HTML extractor cannot trigger TOC shape signals,
        // so seed two `DryrunBookReport`s directly to drive the
        // histogram. One book raises two shape tokens; the second stays
        // clean.
        let raised = DryrunBookReport {
            stem: "raised".to_string(),
            format: "epub".to_string(),
            bytes: 0,
            extract_outcome: "extracted".to_string(),
            adapter: Some("epub".to_string()),
            blocks: Some(200),
            toc_stats: None,
            structure: None,
            chunks: None,
            source_tag: None,
            filename_template: None,
            biblio: None,
            filename_biblio: None,
            base_attrs: None,
            base_attrs_actions: vec![],
            effective: None,
            inherited_from_parent: None,
            audit_fields: vec![],
            audit_shape_flags: vec![
                "toc:unanchored_some".to_string(),
                "toc:unanchored_half".to_string(),
            ],
            verdict: Some("needs_work".to_string()),
            confidence: Some("low".to_string()),
            elapsed_ms: 0,
            error: None,
        };
        let mut clean = raised.clone();
        clean.stem = "clean".to_string();
        clean.audit_shape_flags = vec![];
        clean.verdict = Some("clean".to_string());
        clean.confidence = Some("high".to_string());

        let summary = summarize(&[raised, clean]);
        assert_eq!(
            summary
                .toc_anomaly_counts
                .get("toc:unanchored_some")
                .copied(),
            Some(1)
        );
        assert_eq!(
            summary
                .toc_anomaly_counts
                .get("toc:unanchored_half")
                .copied(),
            Some(1)
        );
        assert_eq!(summary.toc_anomaly_counts.len(), 2);
    }

    #[test]
    fn summarize_aggregates_base_attrs_action_counts() {
        // One book triggers the filename fallback for several fields; a
        // second book stays clean. The histogram should reflect both.
        let dir = tempdir().expect("tempdir");
        let triggered = write_html(
            dir.path(),
            "Alice Author - A Sample Title (2006, Sample Press).html",
            "Body.",
        );
        let clean = write_html(dir.path(), "tiny.html", "Body.");
        let reports = vec![
            dryrun_book(&triggered, &DryrunParams::default()),
            dryrun_book(&clean, &DryrunParams::default()),
        ];
        let summary = summarize(&reports);
        let publisher_fallback = summary
            .base_attrs_action_counts
            .get("filename_fallback:publisher")
            .copied()
            .unwrap_or(0);
        let year_fallback = summary
            .base_attrs_action_counts
            .get("filename_fallback:year")
            .copied()
            .unwrap_or(0);
        assert_eq!(publisher_fallback, 1);
        assert_eq!(year_fallback, 1);
    }

    #[test]
    fn dryrun_path_walks_a_directory() {
        let dir = tempdir().expect("tempdir");
        write_html(dir.path(), "a.html", "Paragraph one.");
        write_html(dir.path(), "b.html", "Paragraph two.");
        // A subdirectory file is also picked up.
        let sub = dir.path().join("nested");
        fs::create_dir(&sub).expect("mkdir");
        write_html(&sub, "c.html", "Paragraph three.");
        let reports = dryrun_path(dir.path(), &DryrunParams::default());
        assert_eq!(reports.len(), 3);
    }
}
