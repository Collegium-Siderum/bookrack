// SPDX-License-Identifier: Apache-2.0

//! The rule front end of the book pipeline: read a rule file's members
//! into an extraction, settle what the catalog already holds for that
//! text and for the PDF it derives from, and hand over to the stages the
//! born-digital path runs after EXTRACT.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bookrack_catalog::{Catalog, CatalogError, Intake, IntakeStatus};
use bookrack_core::{ItemKind, error_chain};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_extract::Extraction;
use bookrack_extract::rule::{RuleFile, RuleKind, UnitsBy};

use crate::pipeline::book::{
    IngestError, IngestParams, IngestReport, OnEmbedBatch, Result, Staged, audit,
    finish_extraction_ingest, new_run_id, noop_report, sha256_hex,
};
use crate::pipeline::common::on_file::{OnFile, on_file};

/// What one [`ingest_rule`] run is told beyond the rule file itself.
/// Neither value is part of the rule, so neither reaches its
/// fingerprint.
#[derive(Debug, Clone, Default)]
pub struct RuleIngestParams {
    /// The scan PDF the rule's text was produced from. When set, the
    /// rule's numbered units are checked against the PDF's pages and
    /// the book is recorded as derived from it.
    pub source_pdf: Option<PathBuf>,
    /// Accept text that leaves pages of [`Self::source_pdf`] without a
    /// unit, recording the pages that are covered.
    pub allow_partial: bool,
}

/// The intake a source PDF is registered as, waiting for OCR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSourceAnchor {
    /// The PDF's intake id.
    pub intake_id: i64,
    /// The SHA-256 of the PDF's bytes, as lowercase hex.
    pub source_sha256: String,
    /// The PDF's page count as recorded when it was registered; `None`
    /// when the count could not be read then.
    pub page_count: Option<u32>,
}

/// Check that `source_pdf` can be named as the source of `rule`'s text.
///
/// The rule has to be able to number its units: it splits by file, or
/// by a marker with a `sheet` group. The PDF has to be a file, registered
/// as a book intake in the `needs_ocr` state. Reads the PDF and the catalog;
/// reads no member of the rule and writes nothing.
pub fn check_rule_source(
    catalog: &Catalog,
    rule: &RuleFile,
    source_pdf: &Path,
) -> Result<RuleSourceAnchor> {
    check_source_shape(rule, source_pdf)?;
    let source_sha256 = sha256_hex(&std::fs::read(source_pdf)?);
    let status = match catalog.intake_by_sha(ItemKind::Book, &source_sha256)? {
        Some(intake) if intake.status == IntakeStatus::NeedsOcr => {
            return Ok(RuleSourceAnchor {
                intake_id: intake.intake_id,
                page_count: intake
                    .page_count
                    .and_then(|pages| u32::try_from(pages).ok()),
                source_sha256,
            });
        }
        Some(intake) => Some(intake.status.as_str()),
        None => None,
    };
    Err(IngestError::RuleSourceNotPending {
        source_pdf: source_pdf.to_path_buf(),
        status,
    })
}

/// The part of [`check_rule_source`] that needs no catalog: the rule can
/// number its units, and `source_pdf` names a file.
fn check_source_shape(rule: &RuleFile, source_pdf: &Path) -> Result<()> {
    if !may_number_units(rule) {
        return Err(IngestError::RuleSourceUnnumbered {
            rule: rule.path().to_path_buf(),
        });
    }
    if !source_pdf.is_file() {
        return Err(IngestError::RuleSourceNotAFile {
            source_pdf: source_pdf.to_path_buf(),
        });
    }
    Ok(())
}

/// Check a rule submission before a job is queued for it.
///
/// The rule file has to parse, describe a book, and name at least one
/// member file. With `source_pdf` set, [`check_rule_source`] has to pass
/// against `catalog`; `None` stands for a library that has no catalog
/// yet, which has registered no PDF and so refuses every source. Reads
/// the rule file, the listing of its directory, the PDF, and the
/// catalog; reads no member's text and writes nothing.
pub fn check_rule_submission(
    rule_path: &Path,
    catalog: Option<&Catalog>,
    source_pdf: Option<&Path>,
) -> Result<()> {
    let rule = RuleFile::load(rule_path)?;
    if rule.rule().kind != RuleKind::Book {
        return Err(IngestError::RuleKindNotBook {
            rule: rule_path.to_path_buf(),
        });
    }
    rule.members()?;
    let Some(source_pdf) = source_pdf else {
        return Ok(());
    };
    match catalog {
        Some(catalog) => check_rule_source(catalog, &rule, source_pdf).map(drop),
        None => {
            check_source_shape(&rule, source_pdf)?;
            Err(IngestError::RuleSourceNotPending {
                source_pdf: source_pdf.to_path_buf(),
                status: None,
            })
        }
    }
}

/// Whether the rule's way of splitting can give a unit a number at all.
fn may_number_units(rule: &RuleFile) -> bool {
    let units = &rule.rule().units;
    match units.by {
        UnitsBy::File => true,
        UnitsBy::Marker => units
            .marker
            .as_deref()
            .and_then(|marker| regex::Regex::new(marker).ok())
            .is_some_and(|marker| marker.capture_names().flatten().any(|name| name == "sheet")),
        UnitsBy::Whole | UnitsBy::Adapter => false,
    }
}

/// Ingest the book a rule file describes end to end: read its members,
/// extract them under the rule, then register, structure, audit, chunk,
/// and embed the result as [`ingest_book`](crate::pipeline::book::ingest_book) does a
/// born-digital file.
///
/// The SHA-256 of the members' canonical text keys the intake. A
/// re-ingest whose stamps and rule fingerprint all match returns a no-op
/// report; one whose fingerprint or extractor stamp moved extracts again
/// and, when the result matches the stored envelope apart from those
/// stamps, rewrites the envelope and advances the stamps alone. Every
/// other re-ingest replaces the corpus tree and vector rows under the
/// same intake id.
///
/// With [`RuleIngestParams::source_pdf`] set, the run is refused before
/// anything is written unless [`check_rule_source`] passes, the units'
/// numbers are checked against the page count recorded for the PDF (read
/// from the PDF itself when none was recorded), and the derivation edge
/// is written once the book is embedded. A text already derived from
/// another PDF is refused before the no-op check. A re-ingest that names
/// no PDF keeps the edge and the partial page set of the run that did.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_rule<E: Embedder>(
    rule_path: &Path,
    corpus: &mut Corpus,
    catalog: &mut Catalog,
    lancedb_dir: &Path,
    books_dir: &Path,
    embedder: &E,
    params: &IngestParams,
    rule_params: &RuleIngestParams,
) -> Result<IngestReport> {
    ingest_rule_with_progress(
        rule_path,
        corpus,
        catalog,
        lancedb_dir,
        books_dir,
        embedder,
        params,
        rule_params,
        &|_, _| {},
    )
    .await
}

/// [`ingest_rule`], reporting each EMBED batch to `on_embed_batch` once
/// it is written.
#[tracing::instrument(
    name = "book",
    skip_all,
    fields(file = %rule_path.display(), intake_id = tracing::field::Empty)
)]
// `ingest_rule`'s eight parameters plus the callback, for the reason
// `ingest_book_with_progress` carries its own.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_rule_with_progress<E: Embedder>(
    rule_path: &Path,
    corpus: &mut Corpus,
    catalog: &mut Catalog,
    lancedb_dir: &Path,
    books_dir: &Path,
    embedder: &E,
    params: &IngestParams,
    rule_params: &RuleIngestParams,
    on_embed_batch: &OnEmbedBatch<'_>,
) -> Result<IngestReport> {
    let rule = RuleFile::load(rule_path)?;
    if rule.rule().kind != RuleKind::Book {
        return Err(IngestError::RuleKindNotBook {
            rule: rule_path.to_path_buf(),
        });
    }
    let anchor = match &rule_params.source_pdf {
        Some(source_pdf) => Some((
            source_pdf.as_path(),
            check_rule_source(catalog, &rule, source_pdf)?,
        )),
        None => None,
    };
    let source = rule.read()?;
    let source_sha = source.source_sha256();
    let fingerprint = rule.fingerprint(&source);
    let run_id = new_run_id(&source_sha);

    // The derivation edge is settled before the no-op check: the PDF and
    // the partial switch belong to the run, not to the rule, so the
    // stamps and the fingerprint cannot see them change.
    let existing = catalog.intake_by_sha(ItemKind::Book, &source_sha)?;
    let edge = settle_edge(
        books_dir,
        existing.as_ref(),
        anchor.as_ref().map(|(_, anchor)| anchor),
        rule_params.allow_partial,
    )?;

    let behind = if params.force || edge.rerun {
        None
    } else {
        match on_file(
            ItemKind::Book,
            catalog,
            corpus,
            &source_sha,
            &params.embed.model,
            Some(&fingerprint),
        )? {
            OnFile::Current(current) => {
                let report = noop_report(current);
                tracing::Span::current().record("intake_id", report.intake_id);
                tracing::info!(
                    intake_id = report.intake_id,
                    "rule ingest noop: text, rule, and stamps current",
                );
                return Ok(report);
            }
            OnFile::ExtractorBehind(intake) | OnFile::RuleChanged(intake) => {
                tracing::Span::current().record("intake_id", intake.intake_id);
                Some(*intake)
            }
            OnFile::Rerun => None,
        }
    };

    // EXTRACT, with the coverage check against the source PDF.
    let started = std::time::Instant::now();
    let extracted =
        tracing::info_span!("operation", stage = "extract").in_scope(|| -> Result<Extraction> {
            let mut extraction = rule.extract(&source, &params.heading_patterns)?;
            for fallback in &extraction.provenance.fallbacks {
                if fallback.kind.starts_with("rule:") {
                    tracing::warn!(
                        kind = %fallback.kind,
                        detail = fallback.detail.as_deref().unwrap_or(""),
                        "rule units are not numbered as the rule expects",
                    );
                }
            }
            match &anchor {
                Some((source_pdf, anchor)) => {
                    // The count recorded with the anchor is the one the
                    // text has to cover; the PDF is read only when the
                    // anchor was registered without one.
                    let expected = match anchor.page_count {
                        Some(pages) => pages,
                        None => bookrack_extract::count_pdf_pages(source_pdf)?,
                    };
                    extraction.provenance.partial_pages = check_coverage(
                        rule_path,
                        source_pdf,
                        &extraction,
                        expected,
                        rule_params.allow_partial,
                    )?;
                    extraction.provenance.derived_from_sha256 = Some(anchor.source_sha256.clone());
                }
                None => {
                    if let Some(inherited) = &edge.inherit {
                        extraction.provenance.derived_from_sha256 =
                            Some(inherited.derived_from_sha256.clone());
                        extraction.provenance.partial_pages = inherited.partial_pages.clone();
                    }
                }
            }
            Ok(extraction)
        });
    let extraction = match extracted {
        Ok(extraction) => extraction,
        Err(e) => {
            audit(
                catalog,
                &run_id,
                &source_sha,
                None,
                "extract",
                "extract",
                "fail",
                started,
                None,
                Some(&error_chain(&e)),
            );
            return Err(e);
        }
    };

    finish_extraction_ingest(
        Staged {
            extraction,
            byte_size: source.byte_size() as i64,
            source_sha,
            original_path: rule_path,
            // A rule file's name describes the rule, not the book.
            source_stem: None,
            run_id,
            started,
            behind,
            hold_for_metadata: false,
        },
        corpus,
        catalog,
        lancedb_dir,
        books_dir,
        embedder,
        params,
        on_embed_batch,
    )
    .await
}

/// What the derivation edge of the row on file asks of a run.
#[derive(Debug, Default)]
struct EdgeDecision {
    /// Run every stage whatever the stamps say.
    rerun: bool,
    /// The edge the fresh extraction carries over from the row on file.
    inherit: Option<InheritedEdge>,
}

/// The derivation recorded by an earlier run that named a source PDF.
#[derive(Debug)]
struct InheritedEdge {
    derived_from_sha256: String,
    partial_pages: Option<Vec<u32>>,
}

/// Weigh the row on file for this text against the source PDF this run
/// names, or does not name.
///
/// A row derived from another PDF is a conflict. A row with no edge yet,
/// or one stored as partial when this run does not allow partial, has to
/// go through the coverage check again. A run that names no PDF carries
/// the edge of the row on file over.
fn settle_edge(
    books_dir: &Path,
    existing: Option<&Intake>,
    anchor: Option<&RuleSourceAnchor>,
    allow_partial: bool,
) -> Result<EdgeDecision> {
    let Some(existing) = existing else {
        return Ok(EdgeDecision::default());
    };
    let stored_partial = || {
        let path = books_dir.join(bookrack_extract::envelope_filename(
            ItemKind::Book,
            existing.intake_id,
        ));
        bookrack_extract::read_envelope_with_fallback(&path)
            .ok()
            .map(|envelope| envelope.extraction.provenance.partial_pages)
    };
    match (anchor, existing.derived_from_sha256.as_deref()) {
        (Some(anchor), Some(edge)) if edge != anchor.source_sha256 => {
            Err(CatalogError::DerivedFromConflict {
                intake_id: existing.intake_id,
                existing: edge.to_string(),
                requested: anchor.source_sha256.clone(),
            }
            .into())
        }
        // An envelope that cannot be read says nothing about how much of
        // the PDF the stored text covers.
        (Some(_), Some(_)) => Ok(EdgeDecision {
            rerun: !allow_partial && stored_partial() != Some(None),
            inherit: None,
        }),
        (Some(_), None) => Ok(EdgeDecision {
            rerun: true,
            inherit: None,
        }),
        (None, Some(edge)) => Ok(EdgeDecision {
            rerun: false,
            inherit: Some(InheritedEdge {
                derived_from_sha256: edge.to_string(),
                partial_pages: stored_partial().flatten(),
            }),
        }),
        (None, None) => Ok(EdgeDecision::default()),
    }
}

/// Match the numbers of the rule's units against the `expected` pages of
/// the source PDF.
///
/// Every unit counts, a unit with no text included: a blank page was
/// still read. Returns the pages covered when some are missing and
/// `allow_partial` is set, `None` when every page is covered. A unit
/// numbered outside `1..=expected` fails the check whatever
/// `allow_partial` says, and so does a unit that holds text and carries
/// no number.
fn check_coverage(
    rule_path: &Path,
    source_pdf: &Path,
    extraction: &Extraction,
    expected: u32,
    allow_partial: bool,
) -> Result<Option<Vec<u32>>> {
    let units = extraction
        .provenance
        .rule
        .as_ref()
        .map(|origin| origin.units.as_slice())
        .unwrap_or_default();
    let numbered: BTreeSet<u32> = units.iter().filter_map(|unit| unit.sheet).collect();
    let text_without_number = extraction.blocks.iter().any(|block| {
        units
            .get(block.source_unit as usize)
            .is_none_or(|unit| unit.sheet.is_none())
    });
    if numbered.is_empty() || text_without_number {
        return Err(IngestError::RuleSourceUnnumbered {
            rule: rule_path.to_path_buf(),
        });
    }
    let (present, excess): (Vec<u32>, Vec<u32>) = numbered
        .into_iter()
        .partition(|number| (1..=expected).contains(number));
    let missing: Vec<u32> = (1..=expected)
        .filter(|page| present.binary_search(page).is_err())
        .collect();
    if !excess.is_empty() || (!missing.is_empty() && !allow_partial) {
        return Err(IngestError::RuleCoverage {
            rule: rule_path.to_path_buf(),
            source_pdf: source_pdf.to_path_buf(),
            covered: present.len(),
            expected,
            missing,
            excess,
        });
    }
    Ok((!missing.is_empty()).then_some(present))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::book::book_pipeline_tests::{Fake, Offline, rewrite_envelope};
    use crate::pipeline::book::rebuild::{RebuildParams, rebuild_from_intakes};
    use crate::pipeline::book::{plan_book_chunks, reaudit};
    use bookrack_catalog::{IntakeFilter, NewIntake};
    use bookrack_core::Explain;
    use bookrack_extract::rule::RULE_ENGINE_VERSION;
    use bookrack_extract::{ExtractionEnvelope, envelope_filename, read_envelope};

    /// Two chapters, one member each, no headings.
    const PLAIN: &str = "\
# A rule for the tests.
schema = 1

[members]
include = [\"*.txt\"]

[units]
by = \"file\"

[structure]
unit_titles = \"none\"
headings = \"none\"

[biblio]
title = \"A Field Guide\"
language = \"en\"
";

    const CHAPTER_ONE: &str = "Chapter one\n\nThe first paragraph of a short sample about birds.\n";
    const CHAPTER_TWO: &str = "Chapter two\n\nA second paragraph carrying more prose to embed.\n";
    const CHAPTER_THREE: &str = "Chapter three\n\nA third paragraph rounding out the sample.\n";

    /// A library in a tempdir and a directory of rule material beside it.
    struct Bench {
        dir: tempfile::TempDir,
        corpus: Corpus,
        catalog: Catalog,
        params: IngestParams,
    }

    impl Bench {
        fn new() -> Bench {
            Bench {
                dir: tempfile::tempdir().expect("tempdir"),
                corpus: Corpus::open_in_memory().expect("corpus"),
                catalog: Catalog::open_in_memory().expect("catalog"),
                params: IngestParams::default(),
            }
        }

        /// A bench whose material is the first `chapters` of the three
        /// sample chapters under [`PLAIN`].
        fn with_chapters(chapters: usize) -> (Bench, PathBuf) {
            let bench = Bench::new();
            for (name, text) in [
                ("01.txt", CHAPTER_ONE),
                ("02.txt", CHAPTER_TWO),
                ("03.txt", CHAPTER_THREE),
            ]
            .into_iter()
            .take(chapters)
            {
                bench.write(name, text);
            }
            let rule = bench.write("rule.toml", PLAIN);
            (bench, rule)
        }

        fn books(&self) -> PathBuf {
            self.dir.path().join("books")
        }

        fn material(&self) -> PathBuf {
            self.dir.path().join("material")
        }

        fn write(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.material().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
            std::fs::write(&path, text).expect("write");
            path
        }

        async fn run(
            &mut self,
            rule: &Path,
            rule_params: &RuleIngestParams,
        ) -> Result<IngestReport> {
            let (vectors, books) = (self.dir.path().join("vectors"), self.books());
            ingest_rule(
                rule,
                &mut self.corpus,
                &mut self.catalog,
                &vectors,
                &books,
                &Fake { dim: 8 },
                &self.params,
                rule_params,
            )
            .await
        }

        async fn plain(&mut self, rule: &Path) -> IngestReport {
            self.run(rule, &RuleIngestParams::default())
                .await
                .expect("rule ingest")
        }

        fn intake(&self, intake_id: i64) -> Intake {
            self.catalog
                .intake_by_id(intake_id)
                .expect("intake")
                .expect("present")
        }

        fn envelope_path(&self, intake_id: i64) -> PathBuf {
            self.books()
                .join(envelope_filename(ItemKind::Book, intake_id))
        }

        fn envelope(&self, intake_id: i64) -> ExtractionEnvelope {
            read_envelope(&self.envelope_path(intake_id)).expect("read envelope")
        }

        /// The fingerprint `rule` has over its members as they stand.
        fn fingerprint(rule: &Path) -> String {
            let file = RuleFile::load(rule).expect("load rule");
            let source = file.read().expect("read members");
            file.fingerprint(&source)
        }

        /// Copy the three-page sample PDF into the bench as `name`, with
        /// `tail` appended so that two copies differ in their hash, and
        /// register it as a scan waiting for OCR with three pages on
        /// record.
        fn pending_pdf(&mut self, name: &str, tail: &[u8]) -> PathBuf {
            self.pending_pdf_recording(name, tail, Some(3))
        }

        /// [`Self::pending_pdf`], recording `pages` as the page count.
        fn pending_pdf_recording(
            &mut self,
            name: &str,
            tail: &[u8],
            pages: Option<i64>,
        ) -> PathBuf {
            let path = self.unregistered_pdf(name, tail);
            let sha = sha256_hex(&std::fs::read(&path).expect("read pdf"));
            let id = self
                .catalog
                .register_intake(ItemKind::Book, &NewIntake::new(sha).format("pdf"))
                .expect("register")
                .intake()
                .intake_id;
            self.catalog
                .set_intake_status(ItemKind::Book, id, IntakeStatus::NeedsOcr)
                .expect("needs_ocr");
            if let Some(pages) = pages {
                self.catalog
                    .set_page_count(ItemKind::Book, id, pages)
                    .expect("page count");
            }
            path
        }

        fn unregistered_pdf(&self, name: &str, tail: &[u8]) -> PathBuf {
            let sample = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../extract/tests/fixtures/rule/polyocr-v1/sample.pdf");
            let mut bytes = std::fs::read(sample).expect("read sample pdf");
            bytes.extend_from_slice(tail);
            let path = self.dir.path().join(name);
            std::fs::write(&path, bytes).expect("write pdf");
            path
        }

        fn pending(&self) -> u64 {
            let awaiting = IntakeFilter {
                awaiting_text: true,
                ..IntakeFilter::default()
            };
            self.catalog
                .count_find_intakes(&awaiting)
                .expect("pending count")
        }
    }

    fn with_source(source_pdf: &Path, allow_partial: bool) -> RuleIngestParams {
        RuleIngestParams {
            source_pdf: Some(source_pdf.to_path_buf()),
            allow_partial,
        }
    }

    /// Whether PDFium is available for counting the sample PDF's pages.
    /// An environment that declares PDFium mandatory fails here instead
    /// of skipping.
    fn pdfium_available() -> bool {
        bookrack_extract::pdfium_gate::available()
    }

    // ---- stamps, fingerprint, and the no-op check ---------------------------

    #[tokio::test]
    async fn a_rule_book_lands_embedded_with_its_fingerprint_and_unit_count() {
        let (mut bench, rule) = Bench::with_chapters(2);
        bench.write("03.txt", " \n");
        let report = bench.plain(&rule).await;
        assert!(report.chunks_written > 0);
        assert!(!report.no_op && !report.restamped && !report.forced);

        let intake = bench.intake(report.intake_id);
        assert_eq!(intake.status, IntakeStatus::Embedded);
        assert_eq!(intake.format.as_deref(), Some("rule-text"));
        assert_eq!(intake.adapter.as_deref(), Some("rule-text"));
        assert_eq!(
            intake.extractor_version,
            bookrack_extract::EXTRACTOR_VERSION
        );
        assert_eq!(
            intake.original_path.as_deref(),
            Some(rule.to_string_lossy().as_ref())
        );
        // The fingerprint outlives the metadata and embed stages, which
        // rewrite the book-state row.
        assert_eq!(
            intake.rule_fingerprint.as_deref(),
            Some(Bench::fingerprint(&rule).as_str()),
        );
        // Three units, the empty one included.
        assert_eq!(intake.page_count, Some(3));
        assert_eq!(intake.derived_from_sha256, None);

        let attrs = bench
            .catalog
            .publication_attrs(report.intake_id, ItemKind::Book)
            .expect("attrs")
            .expect("present");
        assert_eq!(attrs.source.as_deref(), Some("rule"));
        assert_eq!(attrs.source_format.as_deref(), Some("rule-text"));
        assert_eq!(attrs.title.as_deref(), Some("A Field Guide"));
        assert_eq!(report.audit_verdict.as_deref(), Some("clean"));
    }

    #[tokio::test]
    async fn resubmitting_a_rule_unchanged_or_with_new_comments_is_a_noop() {
        let (mut bench, rule) = Bench::with_chapters(2);
        let first = bench.plain(&rule).await;

        let again = bench.plain(&rule).await;
        assert!(again.no_op, "unchanged rule: {again:?}");
        assert_eq!(again.intake_id, first.intake_id);

        bench.write(
            "rule.toml",
            &format!("# Another comment.\n\n{PLAIN}\n# And a trailing one.\n"),
        );
        let commented = bench.plain(&rule).await;
        assert!(commented.no_op, "comments only: {commented:?}");
        assert_eq!(commented.intake_id, first.intake_id);
    }

    #[tokio::test]
    async fn a_rule_change_that_moves_the_product_reruns_under_the_same_intake() {
        let (mut bench, rule) = Bench::with_chapters(2);
        let first = bench.plain(&rule).await;

        bench.write(
            "rule.toml",
            &PLAIN.replace(
                "headings = \"none\"",
                "headings = \"patterns\"\npatterns = [{ regex = '^Chapter \\w+$', depth = 0 }]",
            ),
        );
        let second = bench.plain(&rule).await;
        assert_eq!(second.intake_id, first.intake_id);
        assert!(
            !second.no_op && !second.restamped,
            "a changed table of contents must rerun: {second:?}"
        );
        // The chapter lines are headings now, not prose.
        assert!(second.prose_leaves < first.prose_leaves, "{second:?}");
        assert_eq!(
            bench.intake(first.intake_id).rule_fingerprint.as_deref(),
            Some(Bench::fingerprint(&rule).as_str()),
        );
    }

    /// One unnumbered member: splitting by file and reading the members
    /// as a whole give the same blocks and the same single unit.
    fn single_member() -> (Bench, PathBuf) {
        let bench = Bench::new();
        bench.write("book.txt", CHAPTER_ONE);
        let rule = bench.write("rule.toml", PLAIN);
        (bench, rule)
    }

    fn split_as_whole(bench: &Bench) {
        bench.write(
            "rule.toml",
            &PLAIN.replace("by = \"file\"", "by = \"whole\""),
        );
    }

    #[tokio::test]
    async fn a_rule_change_that_keeps_the_product_only_advances_the_fingerprint() {
        let (mut bench, rule) = single_member();
        let first = bench.plain(&rule).await;
        let before = Bench::fingerprint(&rule);

        split_as_whole(&bench);
        let after = Bench::fingerprint(&rule);
        assert_ne!(before, after, "the fixture must move the fingerprint");

        let second = bench.plain(&rule).await;
        assert!(second.restamped, "same product: {second:?}");
        assert!(!second.no_op);
        assert_eq!(second.intake_id, first.intake_id);
        assert_eq!(second.chunks_written, 0);
        assert_eq!(
            bench.intake(first.intake_id).rule_fingerprint.as_deref(),
            Some(after.as_str())
        );
        let origin = bench
            .envelope(first.intake_id)
            .extraction
            .provenance
            .rule
            .expect("rule origin");
        assert_eq!(origin.fingerprint, after);

        // The fingerprint was written back: the next run has nothing to do.
        let third = bench.plain(&rule).await;
        assert!(third.no_op, "after the restamp: {third:?}");
    }

    #[tokio::test]
    async fn an_engine_version_bump_that_keeps_the_product_only_advances_the_stamps() {
        let (mut bench, rule) = Bench::with_chapters(2);
        let first = bench.plain(&rule).await;
        let current = Bench::fingerprint(&rule);

        // What a run under an earlier engine version left behind.
        bench
            .catalog
            .set_rule_fingerprint(first.intake_id, "fingerprint-of-an-earlier-engine")
            .expect("stale fingerprint");
        rewrite_envelope(&bench.envelope_path(first.intake_id), |envelope| {
            let origin = envelope
                .extraction
                .provenance
                .rule
                .as_mut()
                .expect("origin");
            origin.fingerprint = "fingerprint-of-an-earlier-engine".to_string();
            origin.engine_version = RULE_ENGINE_VERSION - 1;
        });

        let second = bench.plain(&rule).await;
        assert!(second.restamped, "same product: {second:?}");
        assert_eq!(
            bench.intake(first.intake_id).rule_fingerprint.as_deref(),
            Some(current.as_str())
        );
        let origin = bench
            .envelope(first.intake_id)
            .extraction
            .provenance
            .rule
            .expect("rule origin");
        assert_eq!(origin.engine_version, RULE_ENGINE_VERSION);
        assert_eq!(origin.fingerprint, current);

        let third = bench.plain(&rule).await;
        assert!(third.no_op, "after the restamp: {third:?}");
    }

    #[tokio::test]
    async fn a_restamp_for_a_behind_extractor_also_records_a_moved_fingerprint() {
        let (mut bench, rule) = single_member();
        let first = bench.plain(&rule).await;

        let stale = bookrack_extract::EXTRACTOR_VERSION - 1;
        bench
            .catalog
            .set_extraction(ItemKind::Book, first.intake_id, "rule-text", stale)
            .expect("stale extractor stamp");
        rewrite_envelope(&bench.envelope_path(first.intake_id), |envelope| {
            envelope.extraction.provenance.extractor_version = stale;
        });
        split_as_whole(&bench);

        let second = bench.plain(&rule).await;
        assert!(second.restamped, "same product: {second:?}");
        let intake = bench.intake(first.intake_id);
        assert_eq!(
            intake.extractor_version,
            bookrack_extract::EXTRACTOR_VERSION
        );
        assert_eq!(
            intake.rule_fingerprint.as_deref(),
            Some(Bench::fingerprint(&rule).as_str())
        );
    }

    #[tokio::test]
    async fn two_members_merged_into_one_are_not_a_noop() {
        let bench = Bench::new();
        bench.write("01.txt", "Alpha paragraph about herons.\n");
        bench.write("02.txt", "Beta paragraph about egrets.\n");
        let rule = bench.write("rule.toml", PLAIN);
        let mut bench = bench;
        let first = bench.plain(&rule).await;
        assert_eq!(bench.intake(first.intake_id).page_count, Some(2));

        std::fs::remove_file(bench.material().join("02.txt")).expect("remove member");
        bench.write(
            "01.txt",
            "Alpha paragraph about herons.\n\nBeta paragraph about egrets.\n",
        );
        let second = bench.plain(&rule).await;
        // Same canonical text, so the same intake.
        assert_eq!(second.intake_id, first.intake_id);
        assert!(
            !second.no_op && !second.restamped,
            "one unit where there were two must rerun: {second:?}"
        );
        assert_eq!(bench.intake(first.intake_id).page_count, Some(1));
    }

    // ---- metadata ------------------------------------------------------------

    #[tokio::test]
    async fn a_doubtful_rule_book_needs_work() {
        let (mut bench, rule) = Bench::with_chapters(2);
        bench.write(
            "rule.toml",
            &format!("{PLAIN}\n[provenance]\ntext_layer = \"doubtful\"\n"),
        );
        let report = bench.plain(&rule).await;
        assert_eq!(report.audit_verdict.as_deref(), Some("needs_work"));
    }

    #[tokio::test]
    async fn a_reaudit_of_a_rule_book_grades_it_as_its_ingest_did() {
        let bench = Bench::new();
        bench.write("01.txt", CHAPTER_ONE);
        // The rule file is named after the title it states: were its name
        // read as the book's file name, the title would be graded as one
        // that only repeats it.
        let rule = bench.write(
            "A Field Guide.toml",
            &format!("{PLAIN}publisher = \"Reed Press\"\nyear = 2020\n"),
        );
        let mut bench = bench;
        let report = bench.plain(&rule).await;

        let fresh = reaudit::build_report(
            &bench.catalog,
            report.intake_id,
            &bench.params.audit_data,
            &bench.params.audit_profile,
        )
        .expect("reaudit report");
        let title = fresh
            .fields
            .iter()
            .find(|field| field.field == "title")
            .expect("title row");
        assert!(
            title.flags.is_empty(),
            "the rule file's name is not the book's file name: {:?}",
            title.flags
        );
        assert_eq!(
            Some(fresh.verdict.as_token()),
            report.audit_verdict.as_deref()
        );
        assert_eq!(
            Some(fresh.confidence.as_str()),
            report.audit_confidence.as_deref()
        );
    }

    // ---- refusals before anything is written --------------------------------

    #[tokio::test]
    async fn a_reference_rule_is_not_ingested_as_a_book() {
        let (mut bench, rule) = Bench::with_chapters(1);
        bench.write(
            "rule.toml",
            &PLAIN.replace("schema = 1", "schema = 1\nkind = \"reference\""),
        );
        let err = bench
            .run(&rule, &RuleIngestParams::default())
            .await
            .expect_err("a reference rule");
        assert!(
            matches!(err, IngestError::RuleKindNotBook { .. }),
            "{err:?}"
        );
        assert_eq!(
            bench.catalog.count_intakes(ItemKind::Book).expect("count"),
            0
        );
    }

    #[tokio::test]
    async fn a_source_pdf_that_is_not_waiting_for_ocr_is_refused() {
        let (mut bench, rule) = Bench::with_chapters(3);

        let unknown = bench.unregistered_pdf("unknown.pdf", b"");
        let err = bench
            .run(&rule, &with_source(&unknown, false))
            .await
            .expect_err("an unregistered PDF");
        let IngestError::RuleSourceNotPending { status, .. } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(*status, None);
        let unregistered_hint = err.explain().data.hint.expect("hint");

        let known = bench.pending_pdf("known.pdf", b"\n%known");
        let sha = sha256_hex(&std::fs::read(&known).expect("read"));
        let id = bench
            .catalog
            .intake_by_sha(ItemKind::Book, &sha)
            .expect("lookup")
            .expect("present")
            .intake_id;
        bench
            .catalog
            .set_intake_status(ItemKind::Book, id, IntakeStatus::Embedded)
            .expect("embedded");
        let err = bench
            .run(&rule, &with_source(&known, false))
            .await
            .expect_err("an embedded PDF");
        let IngestError::RuleSourceNotPending { status, .. } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(*status, Some("embedded"));
        let problem = err.explain();
        assert!(problem.data.detail.expect("detail").contains("embedded"));
        assert_ne!(problem.data.hint.expect("hint"), unregistered_hint);

        // Neither refusal registered the rule's text.
        assert_eq!(
            bench.catalog.count_intakes(ItemKind::Book).expect("count"),
            1
        );
    }

    #[tokio::test]
    async fn a_source_pdf_needs_a_rule_that_numbers_its_units() {
        let (mut bench, rule) = Bench::with_chapters(3);
        let pdf = bench.pending_pdf("scan.pdf", b"");

        split_as_whole(&bench);
        let err = bench
            .run(&rule, &with_source(&pdf, true))
            .await
            .expect_err("one unnumbered unit");
        assert!(
            matches!(err, IngestError::RuleSourceUnnumbered { .. }),
            "{err:?}"
        );

        let marker = PLAIN
            .replace("include = [\"*.txt\"]", "include = [\"pages.md\"]")
            .replace(
                "by = \"file\"",
                "by = \"marker\"\nmarker = '^<!-- page (?P<label>\\S+) -->$'",
            );
        bench.write("pages.md", "<!-- page i -->\nA page of prose.\n");
        bench.write("rule.toml", &marker);
        let err = bench
            .run(&rule, &with_source(&pdf, true))
            .await
            .expect_err("a marker without a sheet group");
        assert!(
            matches!(err, IngestError::RuleSourceUnnumbered { .. }),
            "{err:?}"
        );
        assert_eq!(bench.pending(), 1);
    }

    #[tokio::test]
    async fn a_source_path_that_is_not_a_file_is_refused_with_a_next_step() {
        let (mut bench, rule) = Bench::with_chapters(3);
        let absent = bench.dir.path().join("absent.pdf");
        let err = bench
            .run(&rule, &with_source(&absent, false))
            .await
            .expect_err("a path with nothing behind it");
        assert!(
            matches!(err, IngestError::RuleSourceNotAFile { .. }),
            "{err:?}"
        );
        let problem = err.explain();
        assert!(problem.summary.contains("absent.pdf"), "{problem:?}");
        assert!(problem.data.hint.is_some(), "{problem:?}");
        assert_eq!(
            bench.catalog.count_intakes(ItemKind::Book).expect("count"),
            0
        );
    }

    // ---- the submission check -----------------------------------------------

    #[test]
    fn a_submission_passes_when_the_rule_names_a_book_with_members() {
        let (bench, rule) = Bench::with_chapters(2);
        check_rule_submission(&rule, Some(&bench.catalog), None).expect("a sound rule");
        check_rule_submission(&rule, None, None).expect("no catalog is needed without a source");
    }

    #[test]
    fn a_submission_is_refused_for_a_rule_that_cannot_run() {
        let (bench, rule) = Bench::with_chapters(0);
        let err = check_rule_submission(&rule, None, None).expect_err("no member matches");
        assert!(
            matches!(
                err,
                IngestError::Rule(bookrack_extract::rule::RuleError::NoMembers { .. })
            ),
            "{err:?}"
        );

        bench.write("01.txt", CHAPTER_ONE);
        bench.write(
            "rule.toml",
            &PLAIN.replace("schema = 1", "schema = 1\nkind = \"reference\""),
        );
        let err = check_rule_submission(&rule, None, None).expect_err("a reference rule");
        assert!(
            matches!(err, IngestError::RuleKindNotBook { .. }),
            "{err:?}"
        );

        bench.write("rule.toml", &format!("{PLAIN}\nstray = 1\n"));
        let err = check_rule_submission(&rule, None, None).expect_err("an unknown key");
        assert!(
            matches!(
                err,
                IngestError::Rule(bookrack_extract::rule::RuleError::Parse { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_submission_checks_its_source_against_the_catalog() {
        let (mut bench, rule) = Bench::with_chapters(3);
        let pending = bench.pending_pdf("scan.pdf", b"");
        check_rule_submission(&rule, Some(&bench.catalog), Some(&pending))
            .expect("a scan waiting for OCR");

        let unknown = bench.unregistered_pdf("unknown.pdf", b"\n%unknown");
        let err = check_rule_submission(&rule, Some(&bench.catalog), Some(&unknown))
            .expect_err("an unregistered PDF");
        assert!(
            matches!(err, IngestError::RuleSourceNotPending { status: None, .. }),
            "{err:?}"
        );

        // A library with no catalog has registered nothing.
        let err = check_rule_submission(&rule, None, Some(&pending))
            .expect_err("no catalog, so no registered PDF");
        assert!(
            matches!(err, IngestError::RuleSourceNotPending { status: None, .. }),
            "{err:?}"
        );

        let absent = bench.dir.path().join("absent.pdf");
        for catalog in [Some(&bench.catalog), None] {
            let err = check_rule_submission(&rule, catalog, Some(&absent))
                .expect_err("a path with nothing behind it");
            assert!(
                matches!(err, IngestError::RuleSourceNotAFile { .. }),
                "{err:?}"
            );
        }

        split_as_whole(&bench);
        let err = check_rule_submission(&rule, None, Some(&pending))
            .expect_err("a rule that numbers nothing");
        assert!(
            matches!(err, IngestError::RuleSourceUnnumbered { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_member_with_text_and_no_number_fails_the_coverage_check() {
        let (mut bench, rule) = Bench::with_chapters(3);
        bench.write("intro.txt", "An introduction no page number places.\n");
        let pdf = bench.pending_pdf("scan.pdf", b"");
        let err = bench
            .run(&rule, &with_source(&pdf, true))
            .await
            .expect_err("an unnumbered member");
        assert!(
            matches!(err, IngestError::RuleSourceUnnumbered { .. }),
            "{err:?}"
        );
        assert_eq!(bench.pending(), 1);
    }

    // ---- coverage and the derivation edge ------------------------------------

    #[tokio::test]
    async fn text_covering_every_page_takes_the_source_off_the_pending_list() {
        let (mut bench, rule) = Bench::with_chapters(3);
        let pdf = bench.pending_pdf("scan.pdf", b"");
        assert_eq!(bench.pending(), 1);

        let report = bench
            .run(&rule, &with_source(&pdf, false))
            .await
            .expect("covered");
        let pdf_sha = sha256_hex(&std::fs::read(&pdf).expect("read"));
        assert_eq!(
            bench.intake(report.intake_id).derived_from_sha256,
            Some(pdf_sha.clone())
        );
        let provenance = bench.envelope(report.intake_id).extraction.provenance;
        assert_eq!(provenance.derived_from_sha256, Some(pdf_sha));
        assert_eq!(provenance.partial_pages, None);
        assert_eq!(bench.pending(), 0);

        // Same PDF, same switch: nothing to do, and the edge stays.
        let again = bench
            .run(&rule, &with_source(&pdf, false))
            .await
            .expect("again");
        assert!(again.no_op, "{again:?}");
        assert!(bench.intake(report.intake_id).derived_from_sha256.is_some());
    }

    #[tokio::test]
    async fn a_run_that_fails_after_coverage_leaves_the_source_pending() {
        let (mut bench, rule) = Bench::with_chapters(3);
        let pdf = bench.pending_pdf("scan.pdf", b"");
        let (vectors, books) = (bench.dir.path().join("vectors"), bench.books());
        let err = ingest_rule(
            &rule,
            &mut bench.corpus,
            &mut bench.catalog,
            &vectors,
            &books,
            &Offline,
            &bench.params,
            &with_source(&pdf, false),
        )
        .await
        .expect_err("the embedder is offline");
        assert!(matches!(err, IngestError::Embed(_)), "{err:?}");

        // The text is registered and extracted, but hangs no edge until
        // it is embedded, so the scan keeps its place on the list.
        let sha = RuleFile::load(&rule)
            .and_then(|file| file.read())
            .expect("read")
            .source_sha256();
        let row = bench
            .catalog
            .intake_by_sha(ItemKind::Book, &sha)
            .expect("lookup")
            .expect("registered");
        assert_eq!(row.status, IntakeStatus::Extracted);
        assert_eq!(row.derived_from_sha256, None);
        assert_eq!(bench.pending(), 1);

        // The next run finishes the job and takes the scan off the list.
        bench
            .run(&rule, &with_source(&pdf, false))
            .await
            .expect("embedder back");
        assert_eq!(bench.pending(), 0);
    }

    #[tokio::test]
    async fn a_source_registered_without_a_page_count_is_counted_from_the_pdf() {
        if !pdfium_available() {
            return;
        }
        let (mut bench, rule) = Bench::with_chapters(2);
        let pdf = bench.pending_pdf_recording("scan.pdf", b"", None);
        let err = bench
            .run(&rule, &with_source(&pdf, false))
            .await
            .expect_err("page 3 is missing");
        let IngestError::RuleCoverage { expected, .. } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(*expected, 3);
    }

    #[tokio::test]
    async fn truncated_text_is_refused_without_allow_partial_and_recorded_with_it() {
        let (mut bench, rule) = Bench::with_chapters(2);
        let pdf = bench.pending_pdf("scan.pdf", b"");

        let err = bench
            .run(&rule, &with_source(&pdf, false))
            .await
            .expect_err("page 3 is missing");
        let IngestError::RuleCoverage {
            covered,
            expected,
            missing,
            excess,
            ..
        } = &err
        else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!((*covered, *expected), (2, 3));
        assert_eq!(missing, &[3]);
        assert!(excess.is_empty());
        let problem = err.explain();
        assert!(
            problem.summary.contains("covers 2 of 3 pages"),
            "{problem:?}"
        );
        assert!(
            problem.data.hint.expect("hint").contains("--allow-partial"),
            "the missing-page hint names the switch"
        );
        // The failed run hangs no edge: the source is still pending.
        assert_eq!(bench.pending(), 1);
        assert_eq!(
            bench.catalog.count_intakes(ItemKind::Book).expect("count"),
            1
        );

        let report = bench
            .run(&rule, &with_source(&pdf, true))
            .await
            .expect("partial accepted");
        assert_eq!(
            bench
                .envelope(report.intake_id)
                .extraction
                .provenance
                .partial_pages,
            Some(vec![1, 2])
        );
        assert_eq!(bench.pending(), 0);
    }

    #[tokio::test]
    async fn a_unit_numbered_beyond_the_pdf_is_refused_even_as_partial() {
        let (mut bench, rule) = Bench::with_chapters(2);
        bench.write("04.txt", "A page the three-page PDF does not have.\n");
        let pdf = bench.pending_pdf("scan.pdf", b"");

        let err = bench
            .run(&rule, &with_source(&pdf, true))
            .await
            .expect_err("page 4 is not in the PDF");
        let IngestError::RuleCoverage {
            covered,
            missing,
            excess,
            ..
        } = &err
        else {
            panic!("unexpected error: {err:?}");
        };
        // Three units are numbered, two of them as pages of the PDF.
        assert_eq!(*covered, 2);
        assert_eq!(missing, &[3]);
        assert_eq!(excess, &[4]);
        // The summary says what failed: a count of covered pages would
        // read as the whole story when no page is missing.
        let problem = err.explain();
        assert!(
            problem
                .summary
                .contains("numbers a unit past page 3, the last of"),
            "{problem:?}"
        );
        assert_eq!(err.to_string(), problem.summary);
        assert!(
            !problem.data.hint.expect("hint").contains("--allow-partial"),
            "the switch does not help a unit outside the PDF"
        );
        assert_eq!(bench.pending(), 1);
    }

    #[tokio::test]
    async fn text_derived_from_one_pdf_is_refused_for_another_before_the_noop_check() {
        let (mut bench, rule) = Bench::with_chapters(3);
        let first_pdf = bench.pending_pdf("first.pdf", b"");
        let other_pdf = bench.pending_pdf("other.pdf", b"\n%other");
        let report = bench
            .run(&rule, &with_source(&first_pdf, false))
            .await
            .expect("first");

        // Every stamp is current, so a no-op check that ran first would
        // return a report here.
        let err = bench
            .run(&rule, &with_source(&other_pdf, false))
            .await
            .expect_err("another PDF");
        assert!(
            matches!(
                err,
                IngestError::Catalog(CatalogError::DerivedFromConflict { intake_id, .. })
                    if intake_id == report.intake_id
            ),
            "{err:?}"
        );
        assert_eq!(
            bench.intake(report.intake_id).derived_from_sha256,
            Some(sha256_hex(&std::fs::read(&first_pdf).expect("read")))
        );
    }

    #[tokio::test]
    async fn naming_the_source_on_a_resubmission_runs_coverage_and_hangs_the_edge() {
        let (mut bench, rule) = Bench::with_chapters(3);
        let pdf = bench.pending_pdf("scan.pdf", b"");
        let first = bench.plain(&rule).await;
        assert_eq!(bench.pending(), 1);

        let second = bench
            .run(&rule, &with_source(&pdf, false))
            .await
            .expect("with the source");
        assert!(!second.no_op, "the edge is new: {second:?}");
        assert_eq!(second.intake_id, first.intake_id);
        assert!(bench.intake(first.intake_id).derived_from_sha256.is_some());
        assert_eq!(bench.pending(), 0);
    }

    #[tokio::test]
    async fn dropping_allow_partial_on_a_resubmission_runs_coverage_again() {
        let (mut bench, rule) = Bench::with_chapters(2);
        let pdf = bench.pending_pdf("scan.pdf", b"");
        bench
            .run(&rule, &with_source(&pdf, true))
            .await
            .expect("partial accepted");

        let err = bench
            .run(&rule, &with_source(&pdf, false))
            .await
            .expect_err("page 3 is still missing");
        assert!(matches!(err, IngestError::RuleCoverage { .. }), "{err:?}");

        // Asking for partial again changes nothing.
        let again = bench
            .run(&rule, &with_source(&pdf, true))
            .await
            .expect("partial again");
        assert!(again.no_op, "{again:?}");
    }

    #[tokio::test]
    async fn a_resubmission_without_the_source_keeps_the_edge_and_the_partial_pages() {
        let (mut bench, rule) = Bench::with_chapters(1);
        let pdf = bench.pending_pdf("scan.pdf", b"");
        let first = bench
            .run(&rule, &with_source(&pdf, true))
            .await
            .expect("partial accepted");
        let pdf_sha = sha256_hex(&std::fs::read(&pdf).expect("read"));

        // Comments only, no source named: a no-op that leaves the edge.
        bench.write("rule.toml", &format!("# A comment.\n{PLAIN}"));
        let commented = bench.plain(&rule).await;
        assert!(commented.no_op, "{commented:?}");
        assert_eq!(
            bench.intake(first.intake_id).derived_from_sha256,
            Some(pdf_sha.clone())
        );

        // A fingerprint that moved under the same product, no source
        // named: the comparison must not read the missing source as a
        // change, and the rewritten envelope keeps the edge.
        bench.write(
            "rule.toml",
            &PLAIN.replace(
                "include = [\"*.txt\"]",
                "include = [\"*.txt\"]\nexclude = [\"notes/*.txt\"]",
            ),
        );
        let moved = bench.plain(&rule).await;
        assert!(moved.restamped, "same product: {moved:?}");
        let provenance = bench.envelope(first.intake_id).extraction.provenance;
        assert_eq!(provenance.derived_from_sha256, Some(pdf_sha.clone()));
        assert_eq!(provenance.partial_pages, Some(vec![1]));
        assert_eq!(
            bench.intake(first.intake_id).derived_from_sha256,
            Some(pdf_sha)
        );
        assert_eq!(bench.pending(), 0);
    }

    // ---- what the stored product is good for ---------------------------------

    #[tokio::test]
    async fn a_rule_book_rebuilds_after_its_material_is_gone() {
        let (mut bench, rule) = Bench::with_chapters(2);
        let report = bench.plain(&rule).await;
        std::fs::remove_dir_all(bench.material()).expect("remove material");

        let rebuilt =
            rebuild_from_intakes(&mut bench.corpus, &bench.catalog, &RebuildParams::default())
                .expect("rebuild");
        assert_eq!(rebuilt.rebuilt, [report.intake_id]);
        assert!(rebuilt.missing_envelope.is_empty() && rebuilt.failed.is_empty());
    }

    /// The page-marked fixture chunks to these `norm_chunk_sha256`
    /// values. They are frozen, and move only with a change to the rule
    /// engine's output or to the chunk or normalize version, each of
    /// which is a reason to re-derive them.
    #[tokio::test]
    async fn a_page_marked_file_chunks_to_the_frozen_hashes() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../extract/tests/fixtures/rule/polyocr-v1");
        let mut bench = Bench::new();
        let report = bench.plain(&fixture.join("rule.toml")).await;
        let plans = plan_book_chunks(&bench.corpus, report.book_root_id, &bench.params.chunk)
            .expect("rule chunks");
        let hashes: Vec<&str> = plans
            .iter()
            .map(|plan| plan.norm_chunk_sha256.as_str())
            .collect();
        assert_eq!(
            hashes,
            ["d39bee47ee6e4b4d3e41a41afa5b09b89860450dbf24c4ef2e1524c014f62b68"]
        );
    }
}
