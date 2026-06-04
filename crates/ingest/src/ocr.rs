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

use bookrack_catalog::{Catalog, IntakeStatus, NewIntake};
use bookrack_extract::Extraction;

use crate::envelope;
use crate::{IngestError, Result, sha256_hex};

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
    /// The scan PDF's intake id. Status is [`IntakeStatus::NeedsOcr`],
    /// `page_count` carries the source's expected sheet count.
    pub pdf_intake_id: i64,
    /// The OCR markdown's intake id. Status is currently
    /// [`IntakeStatus::Extracted`] — a later commit advances it to
    /// `Embedded` after CHUNK + EMBED.
    pub ocr_intake_id: i64,
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
    /// The post-check extraction. On a partial run, its
    /// `provenance.partial_pages` carries the present sheet set.
    pub extraction: Extraction,
}

/// Register the source PDF and the OCR markdown as two intakes, run
/// the OCR adapter, and verify the coverage.
///
/// This commit lays down the registration and check steps; later
/// commits extend the same function to write the envelope and to run
/// STRUCTURE / CHUNK / EMBED.
pub fn ingest_ocr_intake(
    catalog: &mut Catalog,
    books_dir: &Path,
    ocr_md_path: &Path,
    source_pdf_path: &Path,
    params: &OcrIngestParams,
) -> Result<OcrIngestReport> {
    // 1. Read and hash both files.
    let ocr_bytes = std::fs::read(ocr_md_path)?;
    let pdf_bytes = std::fs::read(source_pdf_path)?;
    let source_sha_ocr = sha256_hex(&ocr_bytes);
    let source_sha_pdf = sha256_hex(&pdf_bytes);

    // 2. Determine the expected page count: an explicit override wins,
    //    otherwise PDFium reads the source's `/Pages`.
    let expected_pages = match params.expected_pages {
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

    // 5. Run the OCR adapter. The source PDF is passed so /Outline and
    //    /Info are lifted in the same call; the SHA goes into
    //    `Provenance.derived_from_sha256`.
    let mut extraction =
        bookrack_extract::ocr::extract(ocr_md_path, Some(source_pdf_path), Some(&source_sha_pdf))?;

    // 6. Completeness check. A failure aborts the OCR intake; the
    //    scan PDF intake is left as it was (NeedsOcr) so the user can
    //    fix the OCR product and re-try.
    let coverage = match check_coverage(&extraction, expected_pages, params.allow_partial) {
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

    Ok(OcrIngestReport {
        pdf_intake_id,
        ocr_intake_id,
        source_sha_pdf,
        source_sha_ocr,
        expected_pages,
        ocr_page_count,
        envelope_path,
        extraction,
    })
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

    use std::path::PathBuf;

    use bookrack_catalog::{Catalog, NewIntake};
    use tempfile::tempdir;

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

    #[test]
    fn ingest_ocr_intake_registers_both_intakes_on_a_complete_run() {
        if !pdfium_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let books_dir = dir.path().join("books");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let report = ingest_ocr_intake(
            &mut catalog,
            &books_dir,
            &extract_fixture("sample.bookrack-ocr.v1.md"),
            &extract_fixture("sample.pdf"),
            &OcrIngestParams::default(),
        )
        .expect("ingest");

        assert_ne!(report.pdf_intake_id, report.ocr_intake_id);
        assert_eq!(report.expected_pages, 3);
        assert_eq!(report.ocr_page_count, 3);

        // PDF intake: NeedsOcr, page_count = 3, stored_path under books_dir.
        let pdf = catalog
            .intake_by_id(report.pdf_intake_id)
            .expect("lookup")
            .expect("present");
        assert_eq!(pdf.status, IntakeStatus::NeedsOcr);
        assert_eq!(pdf.page_count, Some(3));
        assert_eq!(pdf.format.as_deref(), Some("pdf"));
        let stored = pdf.stored_path.as_deref().expect("stored_path");
        assert!(
            std::path::Path::new(stored).exists(),
            "PDF copied to opaque store at {stored}",
        );
        assert!(stored.ends_with(&format!("{}.pdf", report.pdf_intake_id)));

        // OCR intake: advanced to Extracted by the envelope-and-stamp
        // step, page_count = 3 from the marker scan, adapter stamped
        // with the OCR adapter string, stored_path pointing at the
        // envelope JSON in the opaque store.
        let ocr = catalog
            .intake_by_id(report.ocr_intake_id)
            .expect("lookup")
            .expect("present");
        assert_eq!(ocr.status, IntakeStatus::Extracted);
        assert_eq!(ocr.page_count, Some(3));
        assert_eq!(ocr.format.as_deref(), Some("ocr-markdown"));
        assert_eq!(ocr.adapter.as_deref(), Some("ocr-pages"));
        assert_eq!(ocr.extractor_version, bookrack_extract::OCR_INTAKE_VERSION,);
        let stored_ocr = ocr.stored_path.as_deref().expect("OCR stored_path set");
        let envelope_path = report.envelope_path.as_deref().expect("envelope written");
        assert_eq!(std::path::Path::new(stored_ocr), envelope_path);
        assert!(envelope_path.exists(), "envelope JSON exists on disk");

        // The envelope's body round-trips: opening it returns an
        // ExtractionEnvelope whose extraction equals the in-memory one,
        // and whose source_sha256 is the OCR markdown's hash (not the
        // PDF's — the PDF hash lives in provenance.derived_from_sha256).
        let env = crate::envelope::read_envelope(envelope_path).expect("read envelope");
        assert_eq!(env.intake_id, report.ocr_intake_id);
        assert_eq!(env.source_sha256, report.source_sha_ocr);
        assert_eq!(env.extraction, report.extraction);

        // Complete coverage → partial_pages stays None;
        // derived_from_sha256 echoes the PDF intake's source_sha256.
        assert_eq!(report.extraction.provenance.partial_pages, None);
        assert_eq!(
            report.extraction.provenance.derived_from_sha256.as_deref(),
            Some(report.source_sha_pdf.as_str()),
        );
    }

    #[test]
    fn ingest_ocr_intake_rejects_a_gap_without_allow_partial() {
        if !pdfium_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let books_dir = dir.path().join("books");
        let ocr_md = dir.path().join("partial.md");
        write_ocr_md(&ocr_md, &[1, 3]); // missing sheet 2
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let err = ingest_ocr_intake(
            &mut catalog,
            &books_dir,
            &ocr_md,
            &extract_fixture("sample.pdf"),
            &OcrIngestParams::default(),
        )
        .expect_err("must reject gap");
        match err {
            IngestError::OcrPagesMissing { missing } => assert_eq!(missing, vec![2]),
            other => panic!("unexpected: {other:?}"),
        }

        // The OCR intake was registered but then aborted; the scan PDF
        // intake stays NeedsOcr so a corrected re-ingest can pick it up.
        let pdf_id = catalog
            .intake_by_sha(&{
                let bytes = std::fs::read(extract_fixture("sample.pdf")).expect("read pdf");
                sha256_hex(&bytes)
            })
            .expect("lookup")
            .expect("present pdf");
        assert_eq!(pdf_id.status, IntakeStatus::NeedsOcr);

        let ocr_bytes = std::fs::read(&ocr_md).expect("read ocr");
        let ocr_intake = catalog
            .intake_by_sha(&sha256_hex(&ocr_bytes))
            .expect("lookup")
            .expect("present ocr");
        assert_eq!(ocr_intake.status, IntakeStatus::Aborted);
    }

    #[test]
    fn ingest_ocr_intake_accepts_a_gap_with_allow_partial_and_records_it() {
        if !pdfium_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let books_dir = dir.path().join("books");
        let ocr_md = dir.path().join("partial.md");
        write_ocr_md(&ocr_md, &[1, 3]);
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let report = ingest_ocr_intake(
            &mut catalog,
            &books_dir,
            &ocr_md,
            &extract_fixture("sample.pdf"),
            &OcrIngestParams {
                expected_pages: None,
                allow_partial: true,
            },
        )
        .expect("ingest with allow_partial");

        assert_eq!(report.extraction.provenance.partial_pages, Some(vec![1, 3]),);
        assert_eq!(report.ocr_page_count, 3);
    }

    #[test]
    fn ingest_ocr_intake_refuses_a_pdf_already_in_a_non_needs_ocr_state() {
        if !pdfium_available() {
            return;
        }
        let dir = tempdir().expect("tempdir");
        let books_dir = dir.path().join("books");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        // Pre-register the source PDF as Extracted: the user already
        // ingested it via its text layer, so the OCR route should not
        // happen on top of that.
        let pdf_bytes = std::fs::read(extract_fixture("sample.pdf")).expect("read pdf");
        let sha = sha256_hex(&pdf_bytes);
        let id = catalog
            .register_intake(&NewIntake::new(sha.clone()).format("pdf"))
            .expect("register")
            .intake()
            .intake_id;
        catalog
            .set_intake_status(id, IntakeStatus::Extracted)
            .expect("flip status");

        let err = ingest_ocr_intake(
            &mut catalog,
            &books_dir,
            &extract_fixture("sample.bookrack-ocr.v1.md"),
            &extract_fixture("sample.pdf"),
            &OcrIngestParams::default(),
        )
        .expect_err("must refuse status mismatch");
        match err {
            IngestError::OcrSourceStatusMismatch { intake_id, status } => {
                assert_eq!(intake_id, id);
                assert_eq!(status, "extracted");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
