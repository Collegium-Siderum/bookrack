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
    /// Per-field grades and flags from the metadata audit.
    pub audit_fields: Vec<FieldOut>,
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
        audit_fields: vec![],
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
    let mut attrs = build_base_attrs(intake_id, extraction, Some(&filename_biblio));
    record.source_tag = attrs.source.clone();
    catalog.upsert_publication_attrs(&attrs)?;
    let effective = catalog.effective_publication_attrs(intake_id, BOOK_SCOPE)?;
    let body = body_sample(extraction);
    let report = audit(&AuditInput {
        biblio: &extraction.biblio,
        provenance: &extraction.provenance,
        effective: &effective,
        toc_stats: &structure_report.toc_stats,
        body_sample: &body,
        total_blocks: extraction.blocks.len(),
        source_stem: Some(stem),
        rules: &params.audit_rules,
    });
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
