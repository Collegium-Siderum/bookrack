// SPDX-License-Identifier: Apache-2.0

//! `ingest_ocr_intake` — register the two intakes an OCR product
//! depends on (the scan PDF source and the OCR markdown itself), run
//! the OCR adapter, and verify page coverage against the source.
//!
//! ## Two intakes, by design
//!
//! Per the OCR-intake commitment, an OCR product enters bookrack as a
//! **derived** source manifestation. The scan PDF is registered as its
//! own intake (status [`IntakeStatus::NeedsOcr`]) so its identity and
//! bytes survive the data model; the OCR markdown is a *separate*
//! intake whose [`bookrack_extract::Provenance::derived_from_sha256`]
//! points back to the scan PDF's hash. Re-OCR is another OCR intake,
//! never an in-place mutation.
//!
//! ## Completeness check
//!
//! Silent partial OCR is the most common failure mode of an OCR
//! pipeline that the user runs externally. The completeness check
//! compares the expected page count — from PDFium's `/Pages` read on
//! the source PDF, or from an explicit `expected_pages` override —
//! against the sheet set the OCR adapter recovered, and refuses an
//! intake whose coverage does not match. The user opts in to partial
//! coverage with [`OcrIngestParams::allow_partial`], in which case the
//! present sheet set is recorded into
//! [`bookrack_extract::Provenance::partial_pages`] so downstream
//! readers know which pages are real.

use std::path::{Path, PathBuf};

use bookrack_catalog::{BOOK_SCOPE, Catalog, IntakeStatus, NewIntake, NewItemState};
use bookrack_core::{NodeId, NodeType, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_extract::Extraction;

use crate::envelope;
use crate::{
    IngestError, IngestParams, Result, ingest_structure, new_run_id, resume_from_chunk,
    run_metadata_substep, set_state, sha256_hex,
};

/// Parameters that shape one [`ingest_ocr_intake`] run.
#[derive(Debug, Clone, Default)]
pub struct OcrIngestParams {
    /// When `Some`, override the expected page count rather than read
    /// it from the source PDF. Used when PDFium is unavailable, or
    /// when the source is not a PDF the caller can hand to PDFium.
    pub expected_pages: Option<u32>,
    /// When true, accept an OCR product whose sheet set does not cover
    /// every expected page. The present sheets are recorded into
    /// `Provenance.partial_pages` rather than rejected.
    pub allow_partial: bool,
}

/// Report from one [`ingest_ocr_intake`] run, carrying the state the
/// later pipeline stages need to chain on.
#[derive(Debug, Clone)]
pub struct OcrIngestReport {
    /// `true` when the run short-circuited because the catalog already
    /// holds an embedded OCR intake for these inputs with matching
    /// stamps. The stage counters (`nodes_written`, `prose_leaves`,
    /// `chunks_written`) are zero in that case; `extraction` is
    /// reloaded from the rebuild-cache envelope so the report shape
    /// mirrors a fresh run.
    pub no_op: bool,
    /// `true` when the run was requested with `params.force`. Distinct
    /// from `no_op` (set when the noop short-circuit fired) so the
    /// caller can tell a forced re-run from an idempotent one.
    pub forced: bool,
    /// The scan PDF's intake id. Status is [`IntakeStatus::NeedsOcr`],
    /// `page_count` carries the source's expected sheet count.
    pub pdf_intake_id: i64,
    /// The OCR markdown's intake id. Status is
    /// [`IntakeStatus::Embedded`] on success.
    pub ocr_intake_id: i64,
    /// The OCR book's root node id. Same `partition_idx =
    /// ocr_intake_id` invariant the rest of the pipeline relies on.
    pub book_root_id: NodeId,
    /// The scan PDF's whole-file SHA-256.
    pub source_sha_pdf: String,
    /// The OCR markdown's whole-file SHA-256.
    pub source_sha_ocr: String,
    /// The expected sheet count the completeness check used.
    pub expected_pages: u32,
    /// The highest sheet number the OCR product carries — its own
    /// page count, recorded into the OCR intake's `page_count`.
    pub ocr_page_count: u32,
    /// Where the rebuild-cache envelope was written. `None` when the
    /// write failed (logged as a warning rather than aborting the
    /// run, matching the `ingest_book` convention).
    pub envelope_path: Option<PathBuf>,
    /// Total corpus nodes written for this book, including the root.
    pub nodes_written: usize,
    /// How many of those nodes are prose leaves.
    pub prose_leaves: usize,
    /// How many chunk rows reached the vector store.
    pub chunks_written: usize,
    /// The metadata audit's plausibility verdict for the book's
    /// effective record (`clean` / `needs_work`). `None` when no
    /// audit ran — typically a `trust-source` profile.
    pub audit_verdict: Option<String>,
    /// The audit's confidence in that verdict (`high` / `medium` /
    /// `low`). Paired with `audit_verdict`; both `None` together.
    pub audit_confidence: Option<String>,
    /// The post-check extraction. On a partial run, its
    /// `provenance.partial_pages` carries the present sheet set.
    pub extraction: Extraction,
}

/// Drive the full OCR-intake pipeline: register the two intakes, run
/// the OCR adapter, verify the coverage, write the rebuild-cache
/// envelope, route the extraction through STRUCTURE, seed and audit
/// the metadata, then chain CHUNK → EMBED. On success the OCR intake
/// reaches [`IntakeStatus::Embedded`]; the scan PDF intake stays
/// [`IntakeStatus::NeedsOcr`] as the durable anchor for the source.
/// The audit verdict is bubbled into the report but never gates the
/// pipeline; it is consultative, exactly as on the born-digital path.
///
/// The metadata substep treats the source PDF's filename as the
/// book's filename channel (the OCR markdown's name is an opaque
/// artifact), and stamps `node_publication_attrs.source = "ocr_marker"`
/// because the provenance adapter is the OCR one — the
/// long-reserved placeholder token surfaces here for the first time.
// Matches the shape of `ingest_book`, which sits right at the 7-argument
// limit and only needs the OCR pair (source PDF + OcrIngestParams) on
// top — bundling that into a struct would obscure the parameter parity
// with `ingest_book` without meaningfully simplifying the call sites.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_ocr_intake<E: Embedder>(
    corpus: &mut Corpus,
    catalog: &mut Catalog,
    lancedb_dir: &Path,
    books_dir: &Path,
    ocr_md_path: &Path,
    source_pdf_path: &Path,
    embedder: &E,
    params: &IngestParams,
    ocr_params: &OcrIngestParams,
) -> Result<OcrIngestReport> {
    // 1. Read and hash both files. The OCR product can be a single
    //    markdown file or a polyocr `dir/` output; `read_source`
    //    returns the canonical text either way, and the OCR intake's
    //    sha256 / byte_size are computed over those bytes.
    let ocr_text = bookrack_extract::ocr::read_source(ocr_md_path)?;
    let ocr_bytes = ocr_text.as_bytes();
    let pdf_bytes = std::fs::read(source_pdf_path)?;
    let source_sha_ocr = sha256_hex(ocr_bytes);
    let source_sha_pdf = sha256_hex(&pdf_bytes);

    // 2. Determine the expected page count: an explicit override wins,
    //    otherwise PDFium reads the source's `/Pages`.
    let expected_pages = match ocr_params.expected_pages {
        Some(n) => n,
        None => bookrack_extract::ocr::count_pdf_pages(source_pdf_path)?,
    };

    // 3. Register the scan PDF intake, idempotent on its hash. On a
    //    fresh registration this is where status, page_count, and the
    //    opaque-store copy land; on a re-registration we only verify
    //    the existing status is still on the OCR track.
    let pdf_reg = catalog.register_intake(
        &NewIntake::new(source_sha_pdf.clone())
            .format("pdf")
            .byte_size(pdf_bytes.len() as i64)
            .original_path(source_pdf_path.to_string_lossy().into_owned()),
    )?;
    let pdf_intake_id = pdf_reg.intake().intake_id;
    if pdf_reg.is_new() {
        catalog.set_intake_status(pdf_intake_id, IntakeStatus::NeedsOcr)?;
        catalog.set_page_count(pdf_intake_id, i64::from(expected_pages))?;
        let pdf_stored = books_dir.join(format!("{pdf_intake_id}.pdf"));
        std::fs::create_dir_all(books_dir)?;
        std::fs::write(&pdf_stored, &pdf_bytes)?;
        catalog.set_stored_path(pdf_intake_id, pdf_stored.to_string_lossy().as_ref())?;
    } else if pdf_reg.intake().status != IntakeStatus::NeedsOcr {
        return Err(IngestError::OcrSourceStatusMismatch {
            intake_id: pdf_intake_id,
            status: pdf_reg.intake().status.as_str(),
        });
    }

    // 4. Register the OCR intake itself. Re-registration is silently
    //    idempotent (matches the `ingest_book` convention); status
    //    advances later in this function.
    let ocr_reg = catalog.register_intake(
        &NewIntake::new(source_sha_ocr.clone())
            .format("ocr-markdown")
            .byte_size(ocr_bytes.len() as i64)
            .original_path(ocr_md_path.to_string_lossy().into_owned()),
    )?;
    let ocr_intake_id = ocr_reg.intake().intake_id;

    // 4a. Idempotent fast path: if the OCR intake already reached
    //     `Embedded` with current OCR_INTAKE_VERSION and the configured
    //     embed model, the run has nothing left to do. Mirrors the
    //     short-circuit `ingest_book` runs after registering its intake.
    //     `force` is the operator's opt-out; with it set, drop through
    //     to the full pipeline so a re-extract / re-embed runs.
    if !params.force
        && let Some(report) = ocr_noop_if_up_to_date(
            catalog,
            ocr_intake_id,
            pdf_intake_id,
            &source_sha_pdf,
            &source_sha_ocr,
            expected_pages,
            &params.embed.model,
        )?
    {
        tracing::info!(
            ocr_intake_id = report.ocr_intake_id,
            "intake ocr noop: OCR product unchanged and stamps current",
        );
        return Ok(report);
    }

    // 5. Run the OCR adapter against the in-memory canonical text:
    //    the source PDF is passed so /Outline and /Info are lifted in
    //    the same call; the PDF's SHA goes into
    //    `Provenance.derived_from_sha256`.
    let mut extraction = bookrack_extract::ocr::extract_from_text(
        &ocr_text,
        Some(source_pdf_path),
        Some(&source_sha_pdf),
    )?;

    // 6. Completeness check. A failure aborts the OCR intake; the
    //    scan PDF intake is left as it was (NeedsOcr) so the user can
    //    fix the OCR product and re-try.
    let coverage = match check_coverage(&extraction, expected_pages, ocr_params.allow_partial) {
        Ok(c) => c,
        Err(e) => {
            catalog.set_intake_status(ocr_intake_id, IntakeStatus::Aborted)?;
            return Err(e);
        }
    };
    if let CoverageOutcome::Partial(present) = &coverage {
        extraction.provenance.partial_pages = Some(present.clone());
    }
    let ocr_page_count = present_sheets(&extraction).last().copied().unwrap_or(0);
    catalog.set_page_count(ocr_intake_id, i64::from(ocr_page_count))?;

    // 7. Write the rebuild-cache envelope. Mirrors `ingest_book`
    //    (lib.rs L429-L453): same opaque-store directory, same
    //    filename convention, same non-fatal failure handling — a
    //    failed write only forfeits the rebuild path for this intake,
    //    it does not abort the run. With the envelope on disk, the
    //    OCR intake is reachable from `corpus rebuild --only` without
    //    re-running the OCR adapter.
    let envelope_path = books_dir.join(envelope::envelope_filename(ocr_intake_id));
    let envelope_path = match envelope::write_envelope(
        &envelope_path,
        &extraction,
        ocr_intake_id,
        &source_sha_ocr,
    ) {
        Ok(()) => {
            catalog.set_stored_path(ocr_intake_id, envelope_path.to_string_lossy().as_ref())?;
            Some(envelope_path)
        }
        Err(err) => {
            tracing::warn!(
                intake_id = ocr_intake_id,
                error = %err,
                "failed to write OCR extraction envelope; rebuild path unavailable for this intake"
            );
            None
        }
    };

    // 8. Stamp adapter + extractor_version and advance status to
    //    `Extracted`. The OCR adapter constants live in the extract
    //    crate; using its `ADAPTER` string keeps the format
    //    commitment single-sourced.
    catalog.set_extraction(
        ocr_intake_id,
        bookrack_extract::ocr::ADAPTER,
        bookrack_extract::OCR_INTAKE_VERSION,
    )?;
    catalog.set_intake_status(ocr_intake_id, IntakeStatus::Extracted)?;

    // 9. STRUCTURE: build the corpus node tree from the extraction.
    //    The OCR book joins the same `partition_idx = intake_id`
    //    regime as every other book — its `book_root_id` is the
    //    OCR intake's root, not the scan PDF's.
    let run_id = new_run_id(&source_sha_ocr);
    let structure = ingest_structure(
        corpus,
        ocr_intake_id,
        NodeType::Work,
        &extraction,
        &params.structure,
    )?;
    let book_root_raw = structure.book_root_id.get();
    let parsed_at = catalog.now_iso()?;

    // 10. Book-state row at the `structure` stage, with
    //     `ocr_marker_finished_at` stamped — the OCR-intake-specific
    //     signal the column was reserved for.
    set_state(
        catalog,
        NewItemState::new(book_root_raw, ocr_intake_id, "structure")
            .parsed_at(&parsed_at)
            .ocr_marker_finished_at(&parsed_at),
    );

    // 11. METADATA (non-blocking): seed publication_attrs from the
    //     extraction, parse a filename biblio off the source PDF's
    //     filename (the OCR markdown is an opaque artifact whose name
    //     does not carry book identity), and run the deterministic
    //     audit. `build_base_attrs` writes `source = "ocr_marker"`
    //     because the provenance adapter is the OCR one — the
    //     placeholder the column was reserved for first surfaces here.
    //     The audit verdict is bubbled into the report; it does not
    //     gate the pipeline (CHUNK + EMBED run regardless).
    let pdf_stem = source_pdf_path.file_stem().and_then(|s| s.to_str());
    let filename_biblio = pdf_stem
        .map(|stem| bookrack_metadata::parse_filename(stem, &params.audit_profile.filename_parser));
    let outcome = run_metadata_substep(
        catalog,
        ocr_intake_id,
        book_root_raw,
        &extraction,
        &structure.toc_stats,
        pdf_stem,
        filename_biblio.as_ref(),
        &params.audit_data,
        &params.audit_profile,
        &run_id,
        &source_sha_ocr,
    );
    let (audit_verdict, audit_confidence) = match &outcome {
        Some(o) => (Some(o.verdict.clone()), Some(o.confidence.clone())),
        None => (None, None),
    };

    // 12. CHUNK + EMBED via the shared resume entry point. It writes
    //     its own audit rows, advances `book_state` to `embed`, and
    //     flips the intake's status to `Embedded` on success.
    let embed = resume_from_chunk(
        corpus,
        catalog,
        lancedb_dir,
        embedder,
        params,
        ocr_intake_id,
        structure.book_root_id,
        &run_id,
        &source_sha_ocr,
        &parsed_at,
    )
    .await?;

    Ok(OcrIngestReport {
        no_op: false,
        forced: params.force,
        pdf_intake_id,
        ocr_intake_id,
        book_root_id: structure.book_root_id,
        source_sha_pdf,
        source_sha_ocr,
        expected_pages,
        ocr_page_count,
        envelope_path,
        nodes_written: structure.nodes_written,
        prose_leaves: structure.prose_leaves,
        chunks_written: embed.chunks_written,
        audit_verdict,
        audit_confidence,
        extraction,
    })
}

/// Build the noop report when the OCR intake is already embedded with
/// matching stamps. Returns `None` when any precondition fails — in
/// which case the caller falls through to the full pipeline.
///
/// Mirrors [`noop_if_up_to_date`](crate::noop_if_up_to_date) but on
/// the OCR side: the dimensions checked are the OCR intake's status,
/// `OCR_INTAKE_VERSION`, the configured embed model, and the
/// rebuild-cache envelope's presence (so the reloaded
/// [`Extraction`] is honest, not a stub).
#[allow(clippy::too_many_arguments)]
fn ocr_noop_if_up_to_date(
    catalog: &Catalog,
    ocr_intake_id: i64,
    pdf_intake_id: i64,
    source_sha_pdf: &str,
    source_sha_ocr: &str,
    expected_pages: u32,
    embed_model: &str,
) -> Result<Option<OcrIngestReport>> {
    let Some(ocr_intake) = catalog.intake_by_sha(source_sha_ocr)? else {
        return Ok(None);
    };
    if ocr_intake.status != IntakeStatus::Embedded {
        return Ok(None);
    }
    if ocr_intake.extractor_version != bookrack_extract::OCR_INTAKE_VERSION {
        return Ok(None);
    }
    let book_root_id = PartitionIdx::new(ocr_intake_id).root();
    let Some(state) = catalog.book_state(book_root_id.get())? else {
        return Ok(None);
    };
    if state.embed_model.as_deref() != Some(embed_model) {
        return Ok(None);
    }
    let stored_path = match ocr_intake.stored_path.as_deref() {
        Some(p) => Path::new(p).to_path_buf(),
        None => return Ok(None),
    };
    let envelope = match crate::envelope::read_envelope(&stored_path) {
        Ok(env) => env,
        Err(_) => return Ok(None),
    };
    let extraction = envelope.extraction;
    let ocr_page_count = present_sheets(&extraction).last().copied().unwrap_or(0);
    let attrs = catalog.publication_attrs(ocr_intake_id, BOOK_SCOPE)?;
    let audit_verdict = attrs.as_ref().and_then(|a| a.audit_verdict.clone());
    let audit_confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
    Ok(Some(OcrIngestReport {
        no_op: true,
        forced: false,
        pdf_intake_id,
        ocr_intake_id,
        book_root_id,
        source_sha_pdf: source_sha_pdf.to_string(),
        source_sha_ocr: source_sha_ocr.to_string(),
        expected_pages,
        ocr_page_count,
        envelope_path: Some(stored_path),
        nodes_written: 0,
        prose_leaves: 0,
        chunks_written: 0,
        audit_verdict,
        audit_confidence,
        extraction,
    }))
}

/// Outcome of the coverage comparison: either the OCR product covers
/// every expected sheet, or it covers only some and the caller opted
/// into a partial ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CoverageOutcome {
    Complete,
    Partial(Vec<u32>),
}

/// Compare the OCR product's sheet set against the expected count.
fn check_coverage(
    extraction: &Extraction,
    expected_pages: u32,
    allow_partial: bool,
) -> Result<CoverageOutcome> {
    let present = present_sheets(extraction);
    let expected: std::collections::BTreeSet<u32> = (1..=expected_pages).collect();
    let present_set: std::collections::BTreeSet<u32> = present.iter().copied().collect();
    let missing: Vec<u32> = expected.difference(&present_set).copied().collect();
    let excess: Vec<u32> = present_set.difference(&expected).copied().collect();
    if !excess.is_empty() {
        return Err(IngestError::OcrPagesExcess { excess });
    }
    if missing.is_empty() {
        return Ok(CoverageOutcome::Complete);
    }
    if !allow_partial {
        return Err(IngestError::OcrPagesMissing { missing });
    }
    Ok(CoverageOutcome::Partial(present))
}

/// 1-based sheets present in the extraction, ascending and unique.
fn present_sheets(extraction: &Extraction) -> Vec<u32> {
    let mut set: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    for block in &extraction.blocks {
        set.insert(block.source_unit + 1);
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_extract::{
        Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc,
    };

    fn extraction_with_sheets(sheets: &[u32]) -> Extraction {
        let blocks = sheets
            .iter()
            .map(|sheet| Block {
                kind: BlockKind::Body,
                text: format!("body for sheet {sheet}"),
                source_unit: sheet - 1,
            })
            .collect();
        Extraction {
            blocks,
            toc: Toc::default(),
            biblio: Biblio::default(),
            provenance: Provenance {
                adapter: "ocr-pages".into(),
                extractor_version: 1,
                text_layer_quality: TextLayerQuality::Doubtful,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        }
    }

    #[test]
    fn check_coverage_passes_when_every_expected_sheet_is_present() {
        let ex = extraction_with_sheets(&[1, 2, 3]);
        let outcome = check_coverage(&ex, 3, false).expect("complete");
        assert_eq!(outcome, CoverageOutcome::Complete);
    }

    #[test]
    fn check_coverage_rejects_a_gap_when_partial_is_not_allowed() {
        let ex = extraction_with_sheets(&[1, 3]);
        let err = check_coverage(&ex, 3, false).expect_err("must reject gap");
        match err {
            IngestError::OcrPagesMissing { missing } => assert_eq!(missing, vec![2]),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn check_coverage_accepts_a_gap_when_partial_is_allowed() {
        let ex = extraction_with_sheets(&[1, 3]);
        let outcome = check_coverage(&ex, 3, true).expect("partial");
        assert_eq!(outcome, CoverageOutcome::Partial(vec![1, 3]));
    }

    #[test]
    fn check_coverage_always_rejects_an_excess_sheet() {
        let ex = extraction_with_sheets(&[1, 2, 3, 4]);
        // Even with allow_partial = true, an excess sheet is fatal.
        let err = check_coverage(&ex, 3, true).expect_err("must reject excess");
        match err {
            IngestError::OcrPagesExcess { excess } => assert_eq!(excess, vec![4]),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn check_coverage_lists_every_missing_sheet_in_order() {
        let ex = extraction_with_sheets(&[2, 5]);
        let err = check_coverage(&ex, 6, false).expect_err("must reject gaps");
        match err {
            IngestError::OcrPagesMissing { missing } => assert_eq!(missing, vec![1, 3, 4, 6]),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // --- Path-level tests against the synthetic OCR fixture --------------
    //
    // The OCR fixture sits in the extract crate's tests tree; these
    // tests reference it through a relative path that holds against
    // both `cargo test --workspace` and a focused `-p bookrack-ingest`
    // run. PDFium is required to lift `/Outline` / `/Info` and to read
    // `/Pages`; without it the tests skip cleanly.

    use std::future::Future;
    use std::path::PathBuf;

    use bookrack_catalog::{Catalog, NewIntake};
    use bookrack_corpus::Corpus;
    use bookrack_embed::{Embedder, Result as EmbedResult};
    use tempfile::tempdir;

    /// Constant-vector embedder, sufficient for the EMBED stage to
    /// produce a stable shape. Mirrors the `Fake` embedder the
    /// `ingest_book` tests use.
    struct Fake {
        dim: usize,
    }

    impl Embedder for Fake {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let (dim, n) = (self.dim, texts.len());
            async move { Ok(vec![vec![0.25f32; dim]; n]) }
        }
    }

    /// Tempdir-backed pipeline scaffolding the path-level tests share.
    struct Pipeline {
        _dir: tempfile::TempDir,
        books_dir: PathBuf,
        lancedb_dir: PathBuf,
        corpus: Corpus,
        catalog: Catalog,
        embedder: Fake,
        params: IngestParams,
        ocr_params: OcrIngestParams,
    }

    fn pipeline() -> Pipeline {
        let dir = tempdir().expect("tempdir");
        Pipeline {
            books_dir: dir.path().join("books"),
            lancedb_dir: dir.path().join("vectors"),
            corpus: Corpus::open_in_memory().expect("corpus"),
            catalog: Catalog::open_in_memory().expect("catalog"),
            embedder: Fake { dim: 8 },
            params: IngestParams::default(),
            ocr_params: OcrIngestParams::default(),
            _dir: dir,
        }
    }

    /// Path to a fixture file under the extract crate's OCR v1 fixture
    /// dir, resolved relative to this crate's manifest dir.
    fn extract_fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../extract/tests/fixtures/ocr/v1")
            .join(name)
    }

    /// Whether PDFium is available — the OCR adapter and
    /// [`count_pdf_pages`] both need it. Pattern mirrors the extract
    /// crate's `pdfium_available`: a real PDF that fails with `Io`
    /// uniquely identifies "binary absent".
    fn pdfium_available() -> bool {
        match bookrack_extract::ocr::count_pdf_pages(&extract_fixture("sample.pdf")) {
            Ok(_) => true,
            Err(bookrack_extract::ExtractError::Io(e)) => {
                eprintln!("skipping ingest-OCR test: PDFium unavailable ({e})");
                false
            }
            Err(other) => panic!("unexpected probe failure: {other:?}"),
        }
    }

    /// Write a synthetic OCR markdown product to `path`, with one body
    /// paragraph per sheet listed in `sheets`.
    fn write_ocr_md(path: &std::path::Path, sheets: &[u32]) {
        let mut buf = String::new();
        for &s in sheets {
            buf.push_str(&format!(
                "<!-- page {s} (sheet {s}) -->\n\npage {s} body\n\n"
            ));
        }
        std::fs::write(path, buf).expect("write ocr md");
    }

    #[tokio::test]
    async fn ingest_ocr_intake_advances_to_embedded_on_a_complete_run() {
        if !pdfium_available() {
            return;
        }
        let mut p = pipeline();

        let report = ingest_ocr_intake(
            &mut p.corpus,
            &mut p.catalog,
            &p.lancedb_dir,
            &p.books_dir,
            &extract_fixture("sample.bookrack-ocr.v1.md"),
            &extract_fixture("sample.pdf"),
            &p.embedder,
            &p.params,
            &p.ocr_params,
        )
        .await
        .expect("ingest");

        assert_ne!(report.pdf_intake_id, report.ocr_intake_id);
        assert_eq!(report.expected_pages, 3);
        assert_eq!(report.ocr_page_count, 3);
        assert!(report.nodes_written > 0);
        assert!(report.chunks_written > 0);

        // PDF intake stays NeedsOcr — the durable source anchor.
        let pdf = p
            .catalog
            .intake_by_id(report.pdf_intake_id)
            .expect("lookup")
            .expect("present");
        assert_eq!(pdf.status, IntakeStatus::NeedsOcr);
        assert_eq!(pdf.page_count, Some(3));
        let stored = pdf.stored_path.as_deref().expect("PDF stored_path");
        assert!(std::path::Path::new(stored).exists());
        assert!(stored.ends_with(&format!("{}.pdf", report.pdf_intake_id)));

        // OCR intake reaches Embedded; stored_path is the envelope JSON.
        let ocr = p
            .catalog
            .intake_by_id(report.ocr_intake_id)
            .expect("lookup")
            .expect("present");
        assert_eq!(ocr.status, IntakeStatus::Embedded);
        assert_eq!(ocr.page_count, Some(3));
        assert_eq!(ocr.format.as_deref(), Some("ocr-markdown"));
        assert_eq!(ocr.adapter.as_deref(), Some("ocr-pages"));
        assert_eq!(ocr.extractor_version, bookrack_extract::OCR_INTAKE_VERSION);
        let stored_ocr = ocr.stored_path.as_deref().expect("OCR stored_path set");
        let envelope_path = report.envelope_path.as_deref().expect("envelope written");
        assert_eq!(std::path::Path::new(stored_ocr), envelope_path);

        // The envelope's body round-trips: opening it returns an
        // ExtractionEnvelope whose extraction equals the in-memory one,
        // and whose source_sha256 is the OCR markdown's hash.
        let env = crate::envelope::read_envelope(envelope_path).expect("read envelope");
        assert_eq!(env.intake_id, report.ocr_intake_id);
        assert_eq!(env.source_sha256, report.source_sha_ocr);
        assert_eq!(env.extraction, report.extraction);

        // book_state at `embed`, with `ocr_marker_finished_at` populated
        // (the column was reserved exactly for this signal) and the
        // embed model recorded.
        let state = p
            .catalog
            .book_state(report.book_root_id.get())
            .expect("book_state lookup")
            .expect("book_state present");
        assert_eq!(state.intake_id, report.ocr_intake_id);
        assert_eq!(state.current_stage, "embed");
        assert!(state.ocr_marker_finished_at.is_some());
        assert!(state.parsed_at.is_some());
        assert!(state.embedded_at.is_some());
        assert_eq!(
            state.embed_model.as_deref(),
            Some(p.params.embed.model.as_str())
        );

        // Complete coverage → partial_pages stays None;
        // derived_from_sha256 echoes the PDF intake's source_sha256.
        assert_eq!(report.extraction.provenance.partial_pages, None);
        assert_eq!(
            report.extraction.provenance.derived_from_sha256.as_deref(),
            Some(report.source_sha_pdf.as_str()),
        );

        // METADATA ran on the OCR book: the audit produced a verdict
        // and `node_publication_attrs.source` is the OCR-marker token —
        // the placeholder the column was reserved for, first consumed
        // by this path.
        assert!(report.audit_verdict.is_some(), "audit verdict produced");
        assert!(
            report.audit_confidence.is_some(),
            "audit confidence produced"
        );
        let attrs = p
            .catalog
            .publication_attrs(report.ocr_intake_id, bookrack_catalog::BOOK_SCOPE)
            .expect("publication_attrs lookup")
            .expect("publication_attrs row present");
        assert_eq!(attrs.source.as_deref(), Some("ocr_marker"));
        assert_eq!(attrs.source_format.as_deref(), Some("ocr-pages"));
        // /Info Title flows in via the source PDF as the title.
        assert_eq!(attrs.title.as_deref(), Some("Synthetic OCR Fixture"));
    }

    #[tokio::test]
    async fn ingest_ocr_intake_rejects_a_gap_without_allow_partial() {
        if !pdfium_available() {
            return;
        }
        let mut p = pipeline();
        let ocr_md = p._dir.path().join("partial.md");
        write_ocr_md(&ocr_md, &[1, 3]); // missing sheet 2

        let err = ingest_ocr_intake(
            &mut p.corpus,
            &mut p.catalog,
            &p.lancedb_dir,
            &p.books_dir,
            &ocr_md,
            &extract_fixture("sample.pdf"),
            &p.embedder,
            &p.params,
            &p.ocr_params,
        )
        .await
        .expect_err("must reject gap");
        match err {
            IngestError::OcrPagesMissing { missing } => assert_eq!(missing, vec![2]),
            other => panic!("unexpected: {other:?}"),
        }

        // The OCR intake was registered but then aborted; the scan PDF
        // intake stays NeedsOcr so a corrected re-ingest can pick it up.
        let pdf_id = p
            .catalog
            .intake_by_sha(&{
                let bytes = std::fs::read(extract_fixture("sample.pdf")).expect("read pdf");
                sha256_hex(&bytes)
            })
            .expect("lookup")
            .expect("present pdf");
        assert_eq!(pdf_id.status, IntakeStatus::NeedsOcr);

        let ocr_bytes = std::fs::read(&ocr_md).expect("read ocr");
        let ocr_intake = p
            .catalog
            .intake_by_sha(&sha256_hex(&ocr_bytes))
            .expect("lookup")
            .expect("present ocr");
        assert_eq!(ocr_intake.status, IntakeStatus::Aborted);
    }

    #[tokio::test]
    async fn ingest_ocr_intake_accepts_a_gap_with_allow_partial_and_records_it() {
        if !pdfium_available() {
            return;
        }
        let mut p = pipeline();
        let ocr_md = p._dir.path().join("partial.md");
        write_ocr_md(&ocr_md, &[1, 3]);
        p.ocr_params.allow_partial = true;

        let report = ingest_ocr_intake(
            &mut p.corpus,
            &mut p.catalog,
            &p.lancedb_dir,
            &p.books_dir,
            &ocr_md,
            &extract_fixture("sample.pdf"),
            &p.embedder,
            &p.params,
            &p.ocr_params,
        )
        .await
        .expect("ingest with allow_partial");

        assert_eq!(report.extraction.provenance.partial_pages, Some(vec![1, 3]));
        assert_eq!(report.ocr_page_count, 3);
        // Even a partial run still completes the rest of the pipeline:
        // the OCR intake reaches Embedded; its chunks land in vectors.
        let ocr = p
            .catalog
            .intake_by_id(report.ocr_intake_id)
            .expect("lookup")
            .expect("present");
        assert_eq!(ocr.status, IntakeStatus::Embedded);
        assert!(report.chunks_written > 0);
    }

    #[tokio::test]
    async fn ingest_ocr_intake_refuses_a_pdf_already_in_a_non_needs_ocr_state() {
        if !pdfium_available() {
            return;
        }
        let mut p = pipeline();

        // Pre-register the source PDF as Extracted: the user already
        // ingested it via its text layer, so the OCR route should not
        // happen on top of that.
        let pdf_bytes = std::fs::read(extract_fixture("sample.pdf")).expect("read pdf");
        let sha = sha256_hex(&pdf_bytes);
        let id = p
            .catalog
            .register_intake(&NewIntake::new(sha.clone()).format("pdf"))
            .expect("register")
            .intake()
            .intake_id;
        p.catalog
            .set_intake_status(id, IntakeStatus::Extracted)
            .expect("flip status");

        let err = ingest_ocr_intake(
            &mut p.corpus,
            &mut p.catalog,
            &p.lancedb_dir,
            &p.books_dir,
            &extract_fixture("sample.bookrack-ocr.v1.md"),
            &extract_fixture("sample.pdf"),
            &p.embedder,
            &p.params,
            &p.ocr_params,
        )
        .await
        .expect_err("must refuse status mismatch");
        match err {
            IngestError::OcrSourceStatusMismatch { intake_id, status } => {
                assert_eq!(intake_id, id);
                assert_eq!(status, "extracted");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn re_ingesting_the_same_ocr_intake_is_a_noop() {
        if !pdfium_available() {
            return;
        }
        let mut p = pipeline();
        let ocr_md = extract_fixture("sample.bookrack-ocr.v1.md");
        let source_pdf = extract_fixture("sample.pdf");

        let first = ingest_ocr_intake(
            &mut p.corpus,
            &mut p.catalog,
            &p.lancedb_dir,
            &p.books_dir,
            &ocr_md,
            &source_pdf,
            &p.embedder,
            &p.params,
            &p.ocr_params,
        )
        .await
        .expect("first ingest");
        assert!(!first.no_op);
        assert!(first.chunks_written > 0);

        let second = ingest_ocr_intake(
            &mut p.corpus,
            &mut p.catalog,
            &p.lancedb_dir,
            &p.books_dir,
            &ocr_md,
            &source_pdf,
            &p.embedder,
            &p.params,
            &p.ocr_params,
        )
        .await
        .expect("re-ingest");

        assert!(second.no_op);
        assert!(!second.forced);
        assert_eq!(second.ocr_intake_id, first.ocr_intake_id);
        assert_eq!(second.pdf_intake_id, first.pdf_intake_id);
        assert_eq!(second.chunks_written, 0);
        assert_eq!(second.nodes_written, 0);
        assert_eq!(second.expected_pages, first.expected_pages);
        assert_eq!(second.extraction, first.extraction);
    }
}
