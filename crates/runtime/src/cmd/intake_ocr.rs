// SPDX-License-Identifier: Apache-2.0

//! `bookrack intake ocr` — register a markdown OCR file as a side
//! intake of an already-ingested PDF, then run STRUCTURE → CHUNK →
//! EMBED over the text layer.

use std::path::Path;

use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig};
use bookrack_corpus::Corpus;
use bookrack_ingest::IngestParams;
use bookrack_ingest::ocr::{OcrIngestParams, ingest_ocr_intake};
use eyre::{Context, Result};

use crate::audit_helpers::{load_audit_data, load_audit_profile, load_heading_patterns};
use crate::embed_helpers::embedder;
use crate::render;

pub async fn run(
    cfg: &Config,
    ocr_md: &Path,
    from_pdf: &Path,
    expected_pages: Option<u32>,
    allow_partial: bool,
    profile_name: Option<&str>,
) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let embedder = embedder(cfg, &embed_cfg)?;
    let audit_data = load_audit_data(cfg);
    let audit_profile = load_audit_profile(cfg, profile_name);
    let heading_patterns = load_heading_patterns(cfg);
    let params = IngestParams {
        embed: embed_cfg,
        audit_data,
        audit_profile,
        heading_patterns,
        ..Default::default()
    };
    let ocr_params = OcrIngestParams {
        expected_pages,
        allow_partial,
    };

    let report = ingest_ocr_intake(
        &mut corpus,
        &mut catalog,
        &cfg.lancedb_dir(),
        &cfg.books_dir(),
        ocr_md,
        from_pdf,
        &embedder,
        &params,
        &ocr_params,
    )
    .await
    .context("ingest OCR")?;

    render::ocr_intake(&report);
    Ok(())
}
